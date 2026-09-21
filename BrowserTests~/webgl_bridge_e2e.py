#!/usr/bin/env python3
"""Browser test for the Unity WebGL bridge without a Unity Editor.

Runs the *real* ``Runtime/Plugins/WebGL/AurixWebGL.jslib`` and the *real* Web SDK browser bundle
(``sdk/web/dist/aurix-web-sdk.js``) in headless Chromium via Playwright, underneath an Emscripten
runtime stand-in (``mergeInto`` / ``LibraryManager`` / UTF-8 heap helpers) — i.e. the JavaScript that a
Unity WebGL player links, driven exactly the way ``Aurix.WebGL.NativeWebGLBridge`` calls it from C#
(pointers to UTF-8 strings in the heap, handle + JSON in, JSON out, ``Drain`` polling). Three layers:

  * always: the WebGL template (``Samples~/WebGLQuickstart/WebGLTemplates/Aurix/index.html``) renders
    with Unity's placeholders substituted and a stub loader, loads the SDK bundle first and shows no
    page errors; the plugin loads the bundle through ``AurixWebGL_LoadSdk`` (status 1 → 2), creates a
    client and fails a connect to a dead endpoint with a clean ``result`` error;
  * with AURIX_API_URL / AURIX_WS_URL / AURIX_API_KEY: a live node — connect, join, roster and
    per-participant stream layout next to a plain Web SDK peer, speaking both ways of silence,
    chat both ways, network quality, stats, mute/volume/pin, leave, disconnect, destroy.

What this does NOT prove: that a Unity build (IL2CPP + Emscripten) links the plugin and marshals the
strings the same way — that needs a Unity Editor and is listed under limitations.

    python3 sdk/unity/BrowserTests~/webgl_bridge_e2e.py [--headed]

Exit code 0 on success, 1 on the first failed check, 3 when the Web SDK bundle is missing.
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

ROOT = Path(__file__).resolve().parents[3]
UNITY = ROOT / "sdk" / "unity"
JSLIB = UNITY / "Runtime" / "Plugins" / "WebGL" / "AurixWebGL.jslib"
TEMPLATE = UNITY / "Samples~" / "WebGLQuickstart" / "WebGLTemplates" / "Aurix" / "index.html"
BUNDLE = ROOT / "sdk" / "web" / "dist" / "aurix-web-sdk.js"

failures = 0
dumped: list = []


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


# Emscripten runtime stand-in + the plugin, loaded as classic scripts so `mergeInto(LibraryManager.library, …)`
# in the .jslib runs untouched. `$Aurix` is hoisted to a global `Aurix`, which is what Emscripten does with
# `$`-prefixed library members the exported functions close over.
EMSCRIPTEN_SHIM = """
var HEAPU8 = new Uint8Array(1 << 22);
var __brk = 8;
var __enc = new TextEncoder(), __dec = new TextDecoder();
function _malloc(size) { var p = __brk; __brk += (size + 7) & ~7; return p; }
function lengthBytesUTF8(s) { return __enc.encode(s).length; }
function stringToUTF8(s, ptr, max) {
  var bytes = __enc.encode(s).subarray(0, Math.max(0, max - 1));
  HEAPU8.set(bytes, ptr); HEAPU8[ptr + bytes.length] = 0;
}
function UTF8ToString(ptr) {
  if (!ptr) return '';
  var end = ptr; while (HEAPU8[end] !== 0) end++;
  return __dec.decode(HEAPU8.subarray(ptr, end));
}
var LibraryManager = { library: {} };
var __deps = [];
function autoAddDeps(lib, dep) { __deps.push(dep); }
function mergeInto(target, lib) { Object.assign(target, lib); }
"""

HARNESS_TAIL = """
var Aurix = LibraryManager.library.$Aurix;
// C# side of NativeWebGLBridge: marshal strings through the heap, read returned pointers back.
function __str(s) { var n = lengthBytesUTF8(s) + 1; var p = _malloc(n); stringToUTF8(s, p, n); return p; }
var lib = LibraryManager.library;
window.__bridge = {
  deps: __deps.slice(),
  loadSdk: function (url) { lib.AurixWebGL_LoadSdk(__str(url)); },
  status: function () { return lib.AurixWebGL_SdkStatus(); },
  error: function () { return UTF8ToString(lib.AurixWebGL_SdkError()); },
  create: function (options) { return lib.AurixWebGL_Create(__str(JSON.stringify(options))); },
  invoke: function (h, method, args, rid) {
    return JSON.parse(UTF8ToString(lib.AurixWebGL_Invoke(h, __str(method), __str(JSON.stringify(args || {})), rid || 0)));
  },
  drain: function (h) { return JSON.parse(UTF8ToString(lib.AurixWebGL_Drain(h))); },
  destroy: function (h) { lib.AurixWebGL_Destroy(h); },
};
// Event log the test polls, like AurixWebGLVoiceClient.Update() drains every frame.
window.__events = [];
window.__rid = 0;
window.__pump = function (h) {
  var batch = window.__bridge.drain(h);
  for (var i = 0; i < batch.length; i++) window.__events.push(batch[i]);
  return batch.length;
};
window.__call = function (h, method, args) {
  var rid = ++window.__rid;
  var r = window.__bridge.invoke(h, method, args, rid);
  return { rid: rid, sync: r };
};
"""

STUB_LOADER = """
// Stand-in for Build/*.loader.js: the template only needs createUnityInstance to resolve.
function createUnityInstance(canvas, config, onProgress) {
  onProgress(0.5); onProgress(1);
  window.__unityConfig = config;
  return Promise.resolve({ SendMessage: function () {}, SetFullscreen: function () {}, Quit: function () { return Promise.resolve(); } });
}
"""


def render_template(source: str) -> str:
    """Substitute Unity's template placeholders and drop the #if blocks the way UnityEditor does for a wasm build."""
    values = {
        "PRODUCT_NAME": "Aurix WebGL quick start",
        "BACKGROUND_COLOR": "#1b1f24",
        "WIDTH": "960",
        "HEIGHT": "600",
        "LOADER_FILENAME": "stub.loader.js",
        "DATA_FILENAME": "stub.data",
        "FRAMEWORK_FILENAME": "stub.framework.js",
        "CODE_FILENAME": "stub.wasm",
        "WORKER_FILENAME": "stub.worker.js",
        "MEMORY_FILENAME": "stub.mem",
        "SYMBOLS_FILENAME": "stub.symbols.json",
        "COMPANY_NAME": "Aurix contributors",
        "PRODUCT_VERSION": "1.2.0",
    }
    enabled = {"USE_WASM": True, "USE_THREADS": False, "MEMORY_FILENAME": False, "SYMBOLS_FILENAME": False}
    out = []
    keep = [True]
    for line in source.splitlines(keepends=True):
        stripped = line.strip()
        if stripped.startswith("#if "):
            keep.append(keep[-1] and enabled.get(stripped[4:].strip(), False))
            continue
        if stripped == "#endif":
            keep.pop()
            continue
        if not keep[-1]:
            continue
        for key, value in values.items():
            line = line.replace("{{{ JSON.stringify(%s) }}}" % key, json.dumps(value))
            line = line.replace("{{{ %s }}}" % key, value)
        out.append(line)
    rendered = "".join(out)
    assert "{{{" not in rendered, "unsubstituted placeholder in the WebGL template"
    return rendered


def stage(work: Path) -> None:
    (work / "Build").mkdir(parents=True)
    (work / "Build" / "stub.loader.js").write_text(STUB_LOADER)
    (work / "index.html").write_text(render_template(TEMPLATE.read_text()))
    shutil.copy(BUNDLE, work / "aurix-web-sdk.js")
    (work / "StreamingAssets").mkdir()
    shutil.copy(BUNDLE, work / "StreamingAssets" / "aurix-web-sdk.js")
    (work / "emscripten.js").write_text(EMSCRIPTEN_SHIM)
    shutil.copy(JSLIB, work / "AurixWebGL.jslib.js")
    (work / "harness-tail.js").write_text(HARNESS_TAIL)
    (work / "harness.html").write_text(
        "<!DOCTYPE html><html><head><meta charset='utf-8'><title>AurixWebGL.jslib harness</title></head><body>"
        "<script src='emscripten.js'></script><script src='AurixWebGL.jslib.js'></script><script src='harness-tail.js'></script>"
        "</body></html>"
    )


def ev(page: Page) -> list:
    global dumped
    dumped = page.evaluate("() => window.__events")
    return dumped


def wait_event(page: Page, handle: int, pred, what: str, timeout: float = 30.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        page.evaluate("(h) => window.__pump(h)", handle)
        for e in ev(page):
            if pred(e):
                check(True, what)
                return e
        time.sleep(0.2)
    tail = [e.get("type") for e in ev(page)][-12:]
    check(False, f"{what} (timeout; last event types: {tail})")
    return None


def call(page: Page, handle: int, method: str, args: dict | None = None, timeout: float = 30.0) -> dict:
    """Invoke like C# does: synchronous value or `pending` followed by a `result` event with the same rid."""
    r = page.evaluate("([h, m, a]) => window.__call(h, m, a)", [handle, method, args or {}])
    sync = r["sync"]
    if not sync.get("pending"):
        return sync
    result = wait_event(page, handle, lambda e: e.get("type") == "result" and e.get("rid") == r["rid"], f"{method}: pending → result", timeout)
    return result or {"ok": False, "error": {"message": "timeout"}}


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
            {"external_id": f"unity-webgl-{name}-{int(time.time())}", "display_name": name,
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
  globalThis.__peer = c; globalThis.__chat = [];
  c.on('chatMessage', (m) => globalThis.__chat.push({ from: m.fromUserId, text: m.text }));
  const info = await c.connect();
  const roster = await c.joinChannel(channel);
  return { userId: info.userId, roster: roster.map((p) => p.userId) };
}
"""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--headed", action="store_true")
    args = ap.parse_args()
    if not BUNDLE.exists():
        print(f"{BUNDLE} missing; run `npm ci && npm run build` in sdk/web first", file=sys.stderr)
        return 3

    live = all(os.environ.get(k) for k in ("AURIX_API_URL", "AURIX_WS_URL", "AURIX_API_KEY"))
    work = Path(tempfile.mkdtemp(prefix="aurix-unity-webgl-"))
    stage(work)
    httpd, port = serve(work)
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

            # --- layer 1a: the WebGL template
            print("Unity WebGL template (Samples~/WebGLQuickstart/WebGLTemplates/Aurix)")
            page = context.new_page()
            page.on("console", lambda m: console.append(m.text))
            page.on("pageerror", lambda e: console.append(f"PAGEERROR {e}"))
            page.goto(f"{base}/index.html", wait_until="load")
            page.wait_for_function("() => window.unityInstance !== undefined", timeout=15000)
            check(page.evaluate("() => typeof window.AurixWebSdk === 'object' && typeof window.AurixWebSdk.AurixBridge === 'function'"),
                  "template loads aurix-web-sdk.js before the player (window.AurixWebSdk.AurixBridge)")
            check(page.evaluate("() => document.querySelector('#aurix-note').className") == "", "no error note on a localhost origin")
            check(page.evaluate("() => window.__unityConfig.streamingAssetsUrl") == "StreamingAssets", "template passes streamingAssetsUrl to the player")
            check(page.evaluate("() => document.querySelector('#unity-loading').style.display") == "none", "loading overlay hidden once the instance resolves")
            check(page.evaluate("() => document.title") == "Aurix WebGL quick start", "PRODUCT_NAME placeholder substituted")
            check(not any("PAGEERROR" in c for c in console), "no uncaught page errors in the template")

            # --- layer 1b: the plugin itself
            print("AurixWebGL.jslib under an Emscripten stand-in")
            page.goto(f"{base}/harness.html", wait_until="load")
            check(page.evaluate("() => window.__bridge.deps") == ["$Aurix"], "plugin declares its $Aurix dependency (autoAddDeps)")
            check(page.evaluate("() => window.__bridge.status()") == 0, "SdkStatus is NotLoaded before LoadSdk")
            check(page.evaluate("() => window.__bridge.create({apiUrl:'http://x',wsUrl:'ws://x',token:'t'})") == 0, "Create fails (0) while the SDK is not loaded")
            page.evaluate("() => window.__bridge.loadSdk('StreamingAssets/aurix-web-sdk.js')")
            check(page.evaluate("() => window.__bridge.status()") in (1, 2), "LoadSdk → Loading")
            page.wait_for_function("() => window.__bridge.status() === 2", timeout=15000)
            check(True, "LoadSdk → Ready once the bundle executes")
            check(page.evaluate("() => window.__bridge.error()") == "", "SdkError empty after a successful load")
            page.evaluate("() => window.__bridge.loadSdk('does-not-exist.js')")
            check(page.evaluate("() => window.__bridge.status()") == 2, "LoadSdk is a no-op once Ready")

            dead = page.evaluate("() => window.__bridge.create({apiUrl:'http://127.0.0.1:9',wsUrl:'ws://127.0.0.1:9/ws',token:'dead',autoReconnect:false,requestTimeoutMs:5000})")
            check(dead > 0, f"Create returns a handle ({dead})")
            r = page.evaluate("([h]) => window.__bridge.invoke(h, 'connectionState', {}, 0)", [dead])
            check(r == {"ok": True, "value": "disconnected"}, f"synchronous value call: {r}")
            r = page.evaluate("([h]) => window.__bridge.invoke(h, 'noSuchMethod', {}, 0)", [dead])
            check(r.get("ok") is False and "noSuchMethod" in r["error"]["message"], f"unknown method → error result: {r.get('error', {}).get('message')}")
            r = call(page, dead, "connect", timeout=40)
            check(r.get("ok") is False and bool(r.get("error", {}).get("message")), f"connect to a dead endpoint rejects with a message: {r.get('error', {}).get('message')!r}")
            wait_event(page, dead, lambda e: e.get("type") == "connectionState" and e.get("state") in ("disconnected", "failed"),
                       "connectionState settles to disconnected/failed after the rejected connect", 10)
            page.evaluate("([h]) => window.__bridge.destroy(h)", [dead])
            check(page.evaluate("([h]) => window.__bridge.drain(h)", [dead]) == [], "Drain on a destroyed handle returns []")
            check(not any("PAGEERROR" in c for c in console), "no uncaught page errors in the harness")

            if not live:
                print("live checks skipped: set AURIX_API_URL, AURIX_WS_URL, AURIX_API_KEY to run against a node")
                return 0 if failures == 0 else 1

            # --- layer 2: live node
            print("AurixWebGL.jslib against a live node")
            api = Api()
            ws_url = os.environ["AURIX_WS_URL"]
            channel = api.post("/v1/channels", {"name": f"unity-webgl-{int(time.time())}", "config": {"channel_type": "team"}})["id"]
            unity_token, unity_uid = api.token(channel, "Unity")
            peer_token, peer_uid = api.token(channel, "Peer")
            page.evaluate("() => { window.__events = []; }")

            # The options object is what WebGLClientOptions.ToBridge produces in C#.
            h = page.evaluate(
                "([o]) => window.__bridge.create(o)",
                [{
                    "apiUrl": api.url, "wsUrl": ws_url, "token": unity_token, "refreshToken": False, "joinToken": False,
                    "useTurn": True, "inputGain": 1.0, "localVoiceActivity": True, "pingIntervalMs": 15000,
                    "qualityReportIntervalMs": 5000, "requestTimeoutMs": 10000, "autoReconnect": True, "rawMessages": False,
                    "visemeEvents": False, "participantStreams": 4,
                    "audioConstraints": {"echoCancellation": True, "noiseSuppression": True, "autoGainControl": True, "channelCount": 1},
                }],
            )
            check(h > 0, f"Create with the C# option shape returns a handle ({h})")
            r = call(page, h, "connect", timeout=60)
            check(r.get("ok") is True and r["value"]["userId"] == unity_uid, "connect resolves with the session (userId matches the token)")
            check(r["value"].get("mediaWebRtc") is True or r["value"].get("media") == "webrtc" or "ssrc" in r["value"], f"session info shape: {sorted(r['value'])}")
            wait_event(page, h, lambda e: e.get("type") == "sessionReady", "sessionReady event drained")
            wait_event(page, h, lambda e: e.get("type") == "connectionState" and e.get("state") == "connected", "connectionState connected")

            r = call(page, h, "joinChannel", {"channelId": channel})
            check(r.get("ok") is True and isinstance(r["value"], list), "joinChannel resolves with the roster")
            wait_event(page, h, lambda e: e.get("type") == "channelJoined" and e.get("channelId") == channel, "channelJoined event")
            r = page.evaluate("([h]) => window.__bridge.invoke(h, 'joinedChannels', {}, 0)", [h])
            check(r.get("value") == [channel], "joinedChannels lists the channel")

            peer = context.new_page()
            peer.goto(f"{base}/harness.html", wait_until="load")
            peer.add_script_tag(url=f"{base}/aurix-web-sdk.js")
            peer.wait_for_function("() => globalThis.AurixWebSdk && globalThis.AurixWebSdk.AurixClient")
            boot = peer.evaluate(PEER_BOOT, [peer_token, channel, api.url, ws_url])
            check(unity_uid in boot["roster"], "peer sees the Unity bridge client in the roster")
            e = wait_event(page, h, lambda e: e.get("type") == "participantJoined" and e.get("participant", {}).get("userId") == peer_uid, "participantJoined for the peer")
            check(bool(e) and e["participant"]["displayName"] == "Peer", f"participant shape: {sorted(e['participant']) if e else None}")
            wait_event(page, h, lambda e: e.get("type") == "remoteAudio", "remoteAudio event (browser playback state)", 30)

            peer.evaluate("() => globalThis.__tone(true)")
            wait_event(page, h, lambda e: e.get("type") == "speaking" and e.get("userId") == peer_uid and e.get("speaking") is True, "speaking(true) for the peer", 30)
            wait_event(page, h, lambda e: e.get("type") == "participantStreams" and any(s.get("userId") == peer_uid for s in e.get("streams", [])),
                       "participantStreams layout assigns the speaking peer a dedicated track", 30)
            peer.evaluate("() => globalThis.__tone(false)")
            wait_event(page, h, lambda e: e.get("type") == "speaking" and e.get("userId") == peer_uid and e.get("speaking") is False, "speaking(false) after silence", 30)

            r = call(page, h, "sendMessage", {"channelId": channel, "text": "hello from unity"})
            check(r.get("ok") is True, "sendMessage resolves")
            deadline = time.time() + 15
            got = []
            while time.time() < deadline and not got:
                got = [m for m in peer.evaluate("() => globalThis.__chat") if m["text"] == "hello from unity"]
                time.sleep(0.2)
            check(bool(got) and got[0]["from"] == unity_uid, "peer received the chat message from the bridge client")
            peer.evaluate("(c) => globalThis.__peer.sendMessage(c, 'hello from peer')", channel)
            e = wait_event(page, h, lambda e: e.get("type") == "chatMessage" and e.get("message", {}).get("text") == "hello from peer", "chatMessage event from the peer")
            check(bool(e) and e["message"]["fromUserId"] == peer_uid, "chat message carries fromUserId")

            e = wait_event(page, h, lambda e: e.get("type") == "networkQuality", "networkQuality event", 45)
            q = (e or {}).get("quality", {})
            check(1 <= q.get("bars", 0) <= 5 and "mos" in q, f"quality shape: bars={q.get('bars')} mos={q.get('mos')}")
            r = call(page, h, "getStats")
            check(r.get("ok") is True and "rttMs" in r["value"], f"getStats resolves with WebRTC stats: {sorted(r.get('value', {}))[:8]}")

            r = page.evaluate("([h]) => window.__bridge.invoke(h, 'setMuted', {muted: true}, 0)", [h])
            check(r.get("ok") is True and page.evaluate("([h]) => window.__bridge.invoke(h, 'isMuted', {}, 0).value", [h]) is True, "setMuted(true) → isMuted")
            page.evaluate("([h]) => window.__bridge.invoke(h, 'setMuted', {muted: false}, 0)", [h])
            r = page.evaluate("([h, u]) => window.__bridge.invoke(h, 'setParticipantVolume', {userId: u, volume: 0.5}, 0)", [h, peer_uid])
            check(r.get("ok") is True and page.evaluate("([h, u]) => window.__bridge.invoke(h, 'getParticipantVolume', {userId: u}, 0).value", [h, peer_uid]) == 0.5, "setParticipantVolume / getParticipantVolume")
            r = page.evaluate("([h, u]) => window.__bridge.invoke(h, 'setPinnedParticipants', {userIds: [u]}, 0)", [h, peer_uid])
            check(r.get("ok") is True, f"setPinnedParticipants ok: {r}")
            r = page.evaluate("([h, u]) => window.__bridge.invoke(h, 'isParticipantSpatialized', {userId: u}, 0)", [h, peer_uid])
            check(r.get("ok") is True and isinstance(r.get("value"), bool), "isParticipantSpatialized answers synchronously (team channel → false)")

            r = page.evaluate("([h, c]) => window.__bridge.invoke(h, 'leaveChannel', {channelId: c}, 0)", [h, channel])
            check(r.get("ok") is True, "leaveChannel ok")
            wait_event(page, h, lambda e: e.get("type") == "channelLeft" and e.get("channelId") == channel, "channelLeft event")
            peer_roster = peer.evaluate("(c) => globalThis.__peer.participants(c).map((p) => p.userId)", channel)
            deadline = time.time() + 10
            while time.time() < deadline and unity_uid in peer_roster:
                time.sleep(0.3)
                peer_roster = peer.evaluate("(c) => globalThis.__peer.participants(c).map((p) => p.userId)", channel)
            check(unity_uid not in peer_roster, "peer roster no longer lists the bridge client after leave")

            r = page.evaluate("([h]) => window.__bridge.invoke(h, 'disconnect', {reason: 'test done'}, 0)", [h])
            check(r.get("ok") is True, "disconnect ok")
            wait_event(page, h, lambda e: e.get("type") == "connectionState" and e.get("state") == "disconnected", "connectionState disconnected")
            page.evaluate("([h]) => window.__bridge.destroy(h)", [h])
            check(page.evaluate("([h]) => window.__bridge.invoke(h, 'connectionState', {}, 0)", [h]).get("ok") is False, "calls on a destroyed handle fail cleanly")
            check(not any("PAGEERROR" in c for c in console), "no uncaught page errors during the live run")
            peer.evaluate("() => globalThis.__peer.disconnect()")
            return 0 if failures == 0 else 1
    finally:
        httpd.shutdown()
        shutil.rmtree(work, ignore_errors=True)
        if failures or os.environ.get("AURIX_E2E_DUMP"):
            print("--- drained events ---")
            for e in dumped:
                print("  " + json.dumps(e)[:300])
        if failures:
            print("--- browser console (tail) ---")
            for line in console[-40:]:
                print("  " + line)
        print(f"unity webgl bridge e2e: {'all checks passed' if failures == 0 else f'{failures} check(s) failed'}")


if __name__ == "__main__":
    sys.exit(main())
