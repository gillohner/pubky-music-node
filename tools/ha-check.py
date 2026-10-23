#!/usr/bin/env python3
"""Home Assistant against a real node (STEWARD-COMPAT-HA-MA.md §2 and §4).

Starts a node on the audio fixtures (zones as DLNA renderers, announced over SSDP) and a real
Home Assistant, onboards Home Assistant through its own API, and checks:

  dlna  Home Assistant's `dlna_dmr` finds the zone's renderer by SSDP; its media_player plays a
        URL on the zone, sets the volume, pauses, resumes and stops it, and HA's state follows
        the zone (GENA events). Every step is checked against the node's own zone state.

Home Assistant runs either from a `hass` executable (a virtualenv with the `homeassistant`
package) or, with --docker IMAGE, as the official container on the host network (CI).

Usage: ha-check.py dlna <pm-node> <work dir> (--hass PATH | --docker IMAGE)
"""
import argparse
import http.server
import json
import os
import secrets
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

FIX = Path(__file__).resolve().parent.parent / "crates/pm-audio/tests/fixtures"
NODE_PORT = 7820
MEDIA_PORT = 7821
HA_PORT = 18123
TOKEN = "ci-token"
NODE = f"http://127.0.0.1:{NODE_PORT}"
HA = f"http://127.0.0.1:{HA_PORT}"
NODE_NAME = "PM HA check"


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


def fail(m):
    log(f"FAIL {m}")
    raise SystemExit(1)


def ok(m):
    log(f"ok   {m}")


def req(url, method="GET", body=None, headers=None, form=False, timeout=60):
    data = None
    h = dict(headers or {})
    if body is not None:
        if form:
            data = urllib.parse.urlencode(body).encode()
            h["Content-Type"] = "application/x-www-form-urlencoded"
        else:
            data = json.dumps(body).encode()
            h["Content-Type"] = "application/json"
    r = urllib.request.Request(url, method=method, data=data, headers=h)
    with urllib.request.urlopen(r, timeout=timeout) as resp:
        raw = resp.read()
        return json.loads(raw) if raw else None


def node(path, method="GET", body=None):
    return req(NODE + path, method, body, {"Authorization": f"Bearer {TOKEN}"})


def until(what, fn, timeout=90, every=1.0):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
            last = v
        except Exception as e:  # noqa: BLE001 - keep polling, report the last error
            last = e
        time.sleep(every)
    fail(f"{what} (last: {last})")


def lan_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))
        return s.getsockname()[0]
    except OSError:
        return "127.0.0.1"
    finally:
        s.close()


# ---- the node ------------------------------------------------------------------------------------

def start_node(node_bin, work):
    music = work / "music"
    music.mkdir(parents=True, exist_ok=True)
    for f in FIX.glob("*.flac"):
        shutil.copy(f, music / f.name)
    (work / "node.toml").write_text(
        f'data_dir = "{work}/data"\n'
        f'[library]\nfolders = ["{music}"]\n'
        f'[api]\nlisten = "0.0.0.0:{NODE_PORT}"\ntoken = "{TOKEN}"\nname = "{NODE_NAME}"\nannounce = false\n'
        "[outputs]\ndiscover = false\nupnp = false\ncast = false\nslimproto_port = 0\nairplay = false\n"
        "[remote]\nenabled = false\n"
        "[renderer]\nenabled = true\n"
    )
    logf = open(work / "node.log", "w")
    p = subprocess.Popen([node_bin, "-c", str(work / "node.toml"), "serve"], stdout=logf, stderr=subprocess.STDOUT,
                         env={**os.environ, "RUST_LOG": "info", "PM_NULL_OUTPUT": "realtime"})
    until("the node answers", lambda: node("/v1/info"), 60)
    node("/v1/library/scan", "POST", {})
    until("the node scanned the fixtures", lambda: len(node("/v1/library/tracks")["tracks"]) > 0 or None, 60)
    return p


def serve_media(work):
    root = work / "media"
    root.mkdir(exist_ok=True)
    shutil.copy(FIX / "60_mono_audio.flac", root / "song.flac")

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **k):
            super().__init__(*a, directory=str(root), **k)

        def log_message(self, *a):
            pass

    srv = http.server.ThreadingHTTPServer(("0.0.0.0", MEDIA_PORT), Quiet)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


# ---- Home Assistant ------------------------------------------------------------------------------

def start_ha(args, work):
    cfg = work / "ha"
    cfg.mkdir(parents=True, exist_ok=True)
    (cfg / "configuration.yaml").write_text(
        "default_config:\n"
        f"http:\n  server_port: {HA_PORT}\n"
        "logger:\n  default: warning\n  logs:\n    homeassistant.components.dlna_dmr: debug\n    homeassistant.components.ssdp: info\n"
    )
    logf = open(work / "ha.log", "w")
    if args.docker:
        subprocess.run(["docker", "rm", "-f", "pm-ha-check"], capture_output=True)
        p = subprocess.Popen(["docker", "run", "--rm", "--name", "pm-ha-check", "--network", "host",
                              "-e", "TZ=UTC", "-v", f"{cfg}:/config", args.docker],
                             stdout=logf, stderr=subprocess.STDOUT)
    else:
        p = subprocess.Popen([args.hass, "-c", str(cfg)], stdout=logf, stderr=subprocess.STDOUT)
    until("Home Assistant answers", lambda: req(HA + "/api/onboarding", timeout=5), 600, 3)
    return p


def onboard():
    """The owner account through HA's onboarding API, then an access token."""
    client = HA + "/"
    password = secrets.token_hex(12)
    r = req(HA + "/api/onboarding/users", "POST",
            {"client_id": client, "name": "CI", "username": "ci", "password": password, "language": "en"})
    tok = req(HA + "/auth/token", "POST",
              {"grant_type": "authorization_code", "code": r["auth_code"], "client_id": client}, form=True)
    h = {"Authorization": f"Bearer {tok['access_token']}"}
    for step, body in (("core_config", {}), ("analytics", {}), ("integration", {"client_id": client, "redirect_uri": client})):
        try:
            req(HA + f"/api/onboarding/{step}", "POST", body, h)
        except urllib.error.HTTPError:
            pass
    return h


class WS:
    """Home Assistant's websocket API, with the standard library (RFC 6455, text frames only)."""

    def __init__(self, token):
        import base64
        self.s = socket.create_connection(("127.0.0.1", HA_PORT), timeout=30)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((f"GET /api/websocket HTTP/1.1\r\nHost: 127.0.0.1:{HA_PORT}\r\nUpgrade: websocket\r\n"
                        f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += self.s.recv(1)
        if b" 101 " not in head.split(b"\r\n")[0]:
            fail(f"websocket refused: {head[:80]!r}")
        self.buf = b""
        self.n = 0
        assert self.recv()["type"] == "auth_required"
        self.send({"type": "auth", "access_token": token})
        if self.recv()["type"] != "auth_ok":
            fail("websocket auth refused")

    def _read(self, n):
        while len(self.buf) < n:
            c = self.s.recv(65536)
            if not c:
                raise ConnectionError("websocket closed")
            self.buf += c
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def recv(self):
        data = b""
        while True:
            b0, b1 = self._read(2)
            ln = b1 & 0x7F
            if ln == 126:
                ln = int.from_bytes(self._read(2), "big")
            elif ln == 127:
                ln = int.from_bytes(self._read(8), "big")
            payload = self._read(ln)
            op = b0 & 0x0F
            if op == 9:  # ping
                self._frame(0xA, payload)
                continue
            data += payload
            if b0 & 0x80:
                return json.loads(data)

    def _frame(self, op, payload):
        mask = os.urandom(4)
        n = len(payload)
        hdr = bytes([0x80 | op])
        hdr += bytes([0x80 | n]) if n < 126 else (bytes([0x80 | 126]) + n.to_bytes(2, "big") if n < 65536 else bytes([0x80 | 127]) + n.to_bytes(8, "big"))
        self.s.sendall(hdr + mask + bytes(c ^ mask[i % 4] for i, c in enumerate(payload)))

    def send(self, msg):
        self._frame(1, json.dumps(msg).encode())

    def call(self, msg):
        self.n += 1
        self.send({"id": self.n, **msg})
        while True:
            r = self.recv()
            if r.get("id") == self.n and r.get("type") == "result":
                if not r.get("success"):
                    fail(f"websocket {msg.get('type')}: {r.get('error')}")
                return r.get("result")


def ha(h, path, method="GET", body=None):
    return req(HA + path, method, body, h)


def service(h, domain, name, data):
    return ha(h, f"/api/services/{domain}/{name}", "POST", data)


def state_of(h, entity):
    return ha(h, f"/api/states/{entity}")


# ---- dlna ----------------------------------------------------------------------------------------

def check_dlna(args, work):
    zones = node("/v1/zones")
    if not zones:
        fail("the node has no zone")
    zone = zones[0]
    zid, zname = zone["id"], zone["name"]
    log(f"zone {zid} ({zname})")
    h = onboard()
    ok("Home Assistant onboarded through its API")

    # 1. Discovery: an SSDP-started dlna_dmr flow for this node's renderer
    def ours(f):
        if f.get("handler") != "dlna_dmr":
            return False
        ctx = json.dumps(f.get("context", {}))
        return f":{NODE_PORT}/upnp/zone/" in ctx or zname in ctx

    ws = WS(h["Authorization"].split(" ", 1)[1])

    def discovered():
        flows = ws.call({"type": "config_entries/flow/progress"})
        mine = [f for f in flows if ours(f)]
        return mine[0] if mine else None

    flow = until("dlna_dmr discovers the zone's renderer over SSDP", discovered, 180, 3)
    ok(f"dlna_dmr discovered the renderer (source {flow['context'].get('source')}: {flow['context'].get('title_placeholders', {}).get('name')})")
    r = ha(h, f"/api/config/config_entries/flow/{flow['flow_id']}", "POST", {})
    if r.get("type") == "form":
        r = ha(h, f"/api/config/config_entries/flow/{flow['flow_id']}", "POST", {})
    if r.get("type") != "create_entry":
        fail(f"the dlna_dmr flow didn't create an entry: {r}")
    ok(f"config entry created: {r.get('title')}")

    def entity():
        for s in ha(h, "/api/states"):
            if s["entity_id"].startswith("media_player.") and s["state"] != "unavailable" \
                    and r.get("title", "\0") in s["attributes"].get("friendly_name", ""):
                return s["entity_id"]
        return None

    ent = until("the media_player entity appears", entity, 90, 2)
    st = state_of(h, ent)
    ok(f"{ent}: state {st['state']}, features {st['attributes'].get('supported_features')}")

    def zone_state():
        return node(f"/v1/zones/{zid}")

    # 2. play_media: the zone plays the URL
    url = f"http://{lan_ip()}:{MEDIA_PORT}/song.flac"
    service(h, "media_player", "play_media", {"entity_id": ent, "media_content_id": url, "media_content_type": "music"})
    z = until("the zone plays the URL Home Assistant sent", lambda: (lambda z: z if z["transport"] == "playing" and (z.get("now_playing") or {}).get("item", {}).get("uri") == url else None)(zone_state()), 60)
    ok("play_media: the zone plays it")
    until("HA's state follows: playing", lambda: state_of(h, ent)["state"] == "playing", 30)
    ok("HA's state: playing")
    until("the position moves", lambda: (zone_state().get("now_playing") or {}).get("position_ms", 0) > 1500, 30)
    ok("the position moves")

    # 3. volume: offered exactly when the zone's output has hardware volume (the renderer never
    #    attenuates in software; a bit-perfect zone stays bit-perfect)
    out = next(o for o in node("/v1/outputs") if o["id"] == zone["output"])
    hw = out["caps"].get("hw_volume") is not None
    has_vol = bool(st["attributes"].get("supported_features", 0) & 4)
    if hw != has_vol:
        fail(f"volume offered {has_vol}, but the output's hardware volume is {hw}")
    if hw:
        service(h, "media_player", "volume_set", {"entity_id": ent, "volume_level": 0.3})
        vol = until("HA's volume follows", lambda: (lambda v: v if v is not None and abs(v - 0.3) < 0.02 else None)(state_of(h, ent)["attributes"].get("volume_level")), 30)
        ok(f"volume_set: HA reports {vol}")
    else:
        ok("no volume control offered: the zone's output has no hardware volume (as designed)")

    # 4. pause, play, stop
    service(h, "media_player", "media_pause", {"entity_id": ent})
    until("the zone pauses", lambda: zone_state()["transport"] == "paused", 30)
    until("HA's state follows: paused", lambda: state_of(h, ent)["state"] == "paused", 30)
    ok("media_pause: zone and HA paused")
    service(h, "media_player", "media_play", {"entity_id": ent})
    until("the zone resumes", lambda: zone_state()["transport"] == "playing", 30)
    until("HA's state follows: playing", lambda: state_of(h, ent)["state"] == "playing", 30)
    ok("media_play: zone and HA playing")
    service(h, "media_player", "media_stop", {"entity_id": ent})
    until("the zone stops", lambda: zone_state()["transport"] == "stopped", 30)
    until("HA's state follows: idle or on", lambda: state_of(h, ent)["state"] in ("idle", "on", "off"), 30)
    ok(f"media_stop: zone stopped, HA {state_of(h, ent)['state']}")

    # 5. A zone started from the node shows in HA (events the other way)
    tracks = node("/v1/library/tracks")["tracks"]
    node(f"/v1/zones/{zid}/queue", "POST", {"track_ids": [tracks[0]["id"]], "replace": True, "play": True})
    until("HA sees the zone play when the node starts it", lambda: state_of(h, ent)["state"] == "playing", 30)
    title = state_of(h, ent)["attributes"].get("media_title")
    ok(f"the node starts the zone: HA shows playing ({title!r})")
    node(f"/v1/zones/{zid}/transport", "POST", {"action": "stop"})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("check", choices=["dlna"])
    ap.add_argument("node_bin")
    ap.add_argument("work")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--hass")
    g.add_argument("--docker")
    args = ap.parse_args()
    work = Path(args.work).resolve()
    if work.exists():
        shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    procs = []
    try:
        serve_media(work)
        procs.append(start_node(args.node_bin, work))
        procs.append(start_ha(args, work))
        if args.check == "dlna":
            check_dlna(args, work)
        log("all Home Assistant checks pass")
    finally:
        if args.docker:
            subprocess.run(["docker", "rm", "-f", "pm-ha-check"], capture_output=True)
        for p in reversed(procs):
            p.terminate()
            try:
                p.wait(20)
            except subprocess.TimeoutExpired:
                p.kill()


if __name__ == "__main__":
    main()
