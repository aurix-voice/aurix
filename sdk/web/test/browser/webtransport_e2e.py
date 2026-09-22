#!/usr/bin/env python3
"""Browser test for AURX over WebTransport in the Web SDK against a live node.

Runs the real browser bundle (``sdk/web/dist/aurix-web-sdk.js``) in headless Chromium via Playwright
and drives three tabs in one channel:

  * Alice — ``transport: 'webtransport'`` (strict, no WebRTC fallback): SessionBind over HTTP/3
    datagrams against the node's hash-pinned certificate, WebCodecs Opus both ways;
  * Bob — ``transport: 'webrtc'``: the classic path, so the test proves the node bridges a
    WebTransport speaker to a WebRTC listener and back;
  * Carol — ``transport: 'auto'``: must pick WebTransport when the node advertises it.

Checks: transport selection and advertisement (URLs, certificate hashes), audio flowing in every
direction (per-tab stats, server ``ChannelEnergy``), mute stopping the uplink, an E2EE channel between
two WebTransport tabs, clean leave/disconnect, and no uncaught page errors.

Needs a node with ``media.webtransport_port`` set (its own short-lived certificate; Chromium accepts the
hash for 127.0.0.1) and TURN enabled for the WebRTC tab (Chromium never gathers loopback host candidates).

    AURIX_API_URL=http://127.0.0.1:8080 AURIX_WS_URL=ws://127.0.0.1:8081/ws AURIX_API_KEY=aurx_… \\
        python3 sdk/web/test/browser/webtransport_e2e.py [--headed]

Exit code 0 on success, 1 on the first failed check, 3 when the bundle is missing.
"""
import argparse
import http.server
import json
import os
import shutil
import socketserver
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import Page, sync_playwright

ROOT = Path(__file__).resolve().parents[4]
BUNDLE = ROOT / "sdk" / "web" / "dist" / "aurix-web-sdk.js"

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

    def channel(self, name: str, **config) -> str:
        return self.post("/v1/channels", {"name": f"wt-{name}-{int(time.time())}", "config": {"channel_type": "team", **config}})["id"]

    def token(self, channels: list[str], name: str) -> tuple[str, str]:
        t = self.post(
            "/v1/tokens",
            {
                "external_id": f"wt-e2e-{name}-{int(time.time() * 1000)}",
                "display_name": name,
                "channels": [{"channel_id": c, "join": True, "speak": True, "receive": True} for c in channels],
            },
        )
        return t["token"], t["user_id"]


BOOT = """
async ([token, apiUrl, wsUrl, transport]) => {
  const { AurixClient } = globalThis.AurixWebSdk;
  const ctx = new AudioContext(); await ctx.resume();
  const dst = ctx.createMediaStreamDestination();
  const osc = ctx.createOscillator(); osc.frequency.value = 330;
  const g = ctx.createGain(); g.gain.value = 0; osc.connect(g).connect(dst); osc.start();
  globalThis.__tone = (on) => { g.gain.value = on ? 0.6 : 0; };
  const c = new AurixClient({
    apiUrl, wsUrl, token, localStream: dst.stream, participantStreams: 4, transport,
    webTransport: { connectTimeoutMs: 8000, heartbeatIntervalMs: 500 },
  });
  globalThis.__client = c;
  globalThis.__energy = {}; globalThis.__left = []; globalThis.__errors = []; globalThis.__transports = [];
  globalThis.__states = [];
  c.on('energy', (ch, levels) => { for (const l of levels) globalThis.__energy[l.user_id] = Math.max(globalThis.__energy[l.user_id] ?? 0, l.energy); });
  c.on('participantLeft', (ch, u) => globalThis.__left.push(u));
  c.on('error', (e) => globalThis.__errors.push(e.message));
  c.on('mediaTransport', (t) => globalThis.__transports.push(t));
  c.on('connectionState', (s) => globalThis.__states.push(s));
  const info = await c.connect();
  return {
    userId: info.userId, sessionId: info.sessionId, state: c.connectionState, transport: c.mediaTransport,
    webTransport: info.webTransport ?? null, transports: globalThis.__transports.slice(),
  };
}
"""

STATS = """
async () => {
  const s = await globalThis.__client.getStats();
  return { transport: s.transport ?? null, packetsSent: s.packetsSent, bytesSent: s.bytesSent,
           packetsReceived: s.packetsReceived, bytesReceived: s.bytesReceived, packetsLost: s.packetsLost,
           iceRttMs: s.iceRttMs, rttMs: s.rttMs, concealedSamples: s.concealedSamples, packetsDiscarded: s.packetsDiscarded };
}
"""


def stats(page: Page) -> dict:
    return page.evaluate(STATS)


def wait_until(what: str, pred, timeout: float = 20.0, interval: float = 0.25):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = pred()
        if last:
            check(True, what)
            return last
        time.sleep(interval)
    check(False, f"{what} (timeout; last {last!r})")
    return None


def grows(page: Page, field: str, seconds: float = 2.0) -> tuple[int, int]:
    a = stats(page)[field]
    time.sleep(seconds)
    b = stats(page)[field]
    return a, b


def boot(context, base: str, token: str, api: Api, ws_url: str, transport: str, console: list[str]) -> tuple[Page, dict]:
    page = context.new_page()
    page.on("console", lambda m: console.append(f"[{transport}] {m.text}"))
    page.on("pageerror", lambda e: console.append(f"PAGEERROR [{transport}] {e}"))
    page.goto(f"{base}/harness.html", wait_until="load")
    page.wait_for_function("() => globalThis.AurixWebSdk && globalThis.AurixWebSdk.AurixClient")
    return page, page.evaluate(BOOT, [token, api.url, ws_url, transport])


def join(page: Page, channel: str) -> list[str]:
    return page.evaluate("async (ch) => (await globalThis.__client.joinChannel(ch)).map((p) => p.userId)", channel)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--headed", action="store_true")
    args = ap.parse_args()
    if not BUNDLE.exists():
        print(f"{BUNDLE} missing; run `npm ci && npm run build` in sdk/web first", file=sys.stderr)
        return 3
    for k in ("AURIX_API_URL", "AURIX_WS_URL", "AURIX_API_KEY"):
        if not os.environ.get(k):
            print(f"{k} is required", file=sys.stderr)
            return 2

    work = Path(tempfile.mkdtemp(prefix="aurix-web-wt-"))
    shutil.copy(BUNDLE, work / "aurix-web-sdk.js")
    (work / "harness.html").write_text(
        "<!DOCTYPE html><html><head><meta charset='utf-8'><title>Aurix WebTransport harness</title></head>"
        "<body><script src='aurix-web-sdk.js'></script></body></html>"
    )
    httpd, port = serve(work)
    base = f"http://127.0.0.1:{port}"
    console: list[str] = []
    api = Api()
    ws_url = os.environ["AURIX_WS_URL"]
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

            print("WebTransport support in this Chromium")
            probe = context.new_page()
            probe.goto(f"{base}/harness.html", wait_until="load")
            probe.wait_for_function("() => globalThis.AurixWebSdk && globalThis.AurixWebSdk.detectWebTransportSupport")
            support = probe.evaluate("() => globalThis.AurixWebSdk.detectWebTransportSupport()")
            check(support.get("webTransport") and support.get("datagrams") and support.get("crypto"), f"WebTransport datagrams + WebCrypto available: {support}")
            probe.close()

            print("Transport selection")
            plain = api.channel("plain")
            e2ee = api.channel("e2ee", e2ee=True)
            alice_tok, alice = api.token([plain, e2ee], "Alice")
            bob_tok, bob = api.token([plain], "Bob")
            carol_tok, carol = api.token([plain, e2ee], "Carol")

            pa, a = boot(context, base, alice_tok, api, ws_url, "webtransport", console)
            check(a["userId"] == alice and a["state"] in ("connected", "media-connected"), f"Alice connected over strict WebTransport ({a['state']})")
            check(a["transport"] == "webtransport" and a["transports"] == ["webtransport"], f"Alice mediaTransport = {a['transport']}, events {a['transports']}")
            wt = a["webTransport"] or {}
            check(bool(wt.get("urls")) and all(u.startswith("https://") and u.endswith("/aurix") for u in wt["urls"]), f"node advertises WebTransport URLs {wt.get('urls')}")
            check(1 <= len(wt.get("certSha256") or []) <= 2 and all(len(h) == 64 for h in wt["certSha256"]), f"advertised certificate hashes: {len(wt.get('certSha256') or [])}")
            check(pa.evaluate("() => globalThis.__errors") == [], f"no client errors while connecting: {pa.evaluate('() => globalThis.__errors')}")

            pb, b = boot(context, base, bob_tok, api, ws_url, "webrtc", console)
            check(b["userId"] == bob and b["transport"] == "webrtc", f"Bob connected over WebRTC ({b['transport']})")
            check(b["webTransport"] is not None, "WebTransport is advertised to a WebRTC tab too (policy decides, not the node)")

            pc, c = boot(context, base, carol_tok, api, ws_url, "auto", console)
            check(c["userId"] == carol and c["transport"] == "webtransport", f"Carol on `auto` picked WebTransport ({c['transport']})")

            print("Plain channel: WebTransport ↔ WebRTC ↔ WebTransport")
            roster_a = join(pa, plain)
            check(roster_a == [], f"Alice joins an empty plain channel (roster excludes the joiner: {roster_a})")
            roster_b = join(pb, plain)
            check(alice in roster_b, f"Bob sees Alice in the roster ({len(roster_b)} entries)")
            roster_c = join(pc, plain)
            check(alice in roster_c and bob in roster_c, f"Carol sees Alice and Bob ({len(roster_c)} entries)")

            sa = wait_until("Alice stats report transport=webtransport with an RTT", lambda: (lambda s: s if s["transport"] == "webtransport" and s["iceRttMs"] >= 0 else None)(stats(pa)))
            check(sa is not None and sa["packetsSent"] > 0, f"Alice uplink datagrams counted: {sa and sa['packetsSent']}")

            pa.evaluate("() => globalThis.__tone(true)")
            wait_until("Bob (WebRTC) hears Alice: server energy for Alice > 0", lambda: pb.evaluate("(u) => globalThis.__energy[u] ?? 0", alice) > 0.01, 20)
            wait_until("Carol (WebTransport) hears Alice: server energy for Alice > 0", lambda: pc.evaluate("(u) => globalThis.__energy[u] ?? 0", alice) > 0.01, 20)
            c0, c1 = grows(pc, "packetsReceived")
            check(c1 - c0 >= 40, f"Carol's WebTransport downlink carries Alice ({c1 - c0} packets in 2 s)")
            b0, b1 = grows(pb, "packetsReceived")
            check(b1 - b0 >= 40, f"Bob's WebRTC downlink carries Alice ({b1 - b0} packets in 2 s)")
            sc = stats(pc)
            check(sc["packetsLost"] == 0 and sc["packetsDiscarded"] == 0, f"Carol: no loss / rejected datagrams (lost {sc['packetsLost']}, discarded {sc['packetsDiscarded']})")
            pa.evaluate("() => globalThis.__tone(false)")

            pb.evaluate("() => globalThis.__tone(true)")
            wait_until("Alice (WebTransport) hears Bob (WebRTC): server energy for Bob > 0", lambda: pa.evaluate("(u) => globalThis.__energy[u] ?? 0", bob) > 0.01, 20)
            a0, a1 = grows(pa, "packetsReceived")
            check(a1 - a0 >= 40, f"Alice's WebTransport downlink carries Bob's WebRTC audio ({a1 - a0} packets in 2 s)")
            pb.evaluate("() => globalThis.__tone(false)")

            print("Mute pauses the WebTransport uplink")
            pa.evaluate("() => globalThis.__tone(true)")
            pa.evaluate("() => globalThis.__client.setMuted(true)")
            time.sleep(0.5)
            m0, m1 = grows(pa, "packetsSent", 1.5)
            check(m1 - m0 <= 4, f"only heartbeats leave while muted ({m1 - m0} datagrams in 1.5 s)")
            pa.evaluate("() => globalThis.__client.setMuted(false)")
            u0, u1 = grows(pa, "packetsSent", 1.5)
            check(u1 - u0 >= 30, f"uplink resumes after unmute ({u1 - u0} packets in 1.5 s)")
            pa.evaluate("() => globalThis.__tone(false)")

            print("E2EE channel between two WebTransport tabs")
            join(pa, e2ee)
            roster = join(pc, e2ee)
            check(alice in roster, "Carol sees Alice in the E2EE channel")
            pa.evaluate("() => globalThis.__tone(true)")
            wait_until("Alice's E2EE frames reach Carol (server energy from the AURX header)", lambda: pc.evaluate("(u) => globalThis.__energy[u] ?? 0", alice) > 0.01, 20)
            e0, e1 = grows(pc, "packetsReceived")
            check(e1 - e0 >= 40, f"Carol receives Alice's E2EE datagrams ({e1 - e0} packets in 2 s)")
            check(pc.evaluate("() => globalThis.__errors") == [], f"Carol: no client errors: {pc.evaluate('() => globalThis.__errors')}")
            pa.evaluate("() => globalThis.__tone(false)")

            print("Leave and disconnect")
            pa.evaluate("async (ch) => { await globalThis.__client.leaveChannel(ch); }", plain)
            wait_until("Bob sees Alice leave the plain channel", lambda: alice in pb.evaluate("() => globalThis.__left"), 10)
            pa.evaluate("() => globalThis.__client.disconnect()")
            wait_until("Alice disconnected cleanly", lambda: pa.evaluate("() => globalThis.__client.connectionState") == "disconnected", 10)
            wait_until("Carol sees Alice leave the E2EE channel", lambda: alice in pc.evaluate("() => globalThis.__left"), 10)
            time.sleep(1.0)
            sc2 = stats(pc)
            check(sc2["transport"] == "webtransport", "Carol's WebTransport session survives a peer leaving")
            pb.evaluate("() => globalThis.__client.disconnect()")
            pc.evaluate("() => globalThis.__client.disconnect()")
            check(pa.evaluate("() => globalThis.__errors") == [], f"Alice: no client errors over the run: {pa.evaluate('() => globalThis.__errors')}")
            check(pb.evaluate("() => globalThis.__errors") == [], f"Bob: no client errors over the run: {pb.evaluate('() => globalThis.__errors')}")
            check(not any(c.startswith("PAGEERROR") for c in console), "no uncaught page errors")
            browser.close()
    finally:
        httpd.shutdown()
        if failures:
            print("--- browser console (tail) ---")
            for line in console[-60:]:
                print(line)
    print("all checks passed" if failures == 0 else f"{failures} check(s) failed")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
