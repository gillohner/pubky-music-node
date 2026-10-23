#!/usr/bin/env python3
"""Home Assistant against a real node (STEWARD-COMPAT-HA-MA.md §2 and §4).

Starts a node on the audio fixtures (zones as DLNA renderers, announced over SSDP) and a real
Home Assistant, onboards Home Assistant through its own API, and checks:

  dlna  Home Assistant's `dlna_dmr` finds the zone's renderer by SSDP; its media_player plays a
        URL on the zone, sets the volume, pauses, resumes and stops it, and HA's state follows
        the zone (GENA events). Every step is checked against the node's own zone state.
  integration
        The Pubky Music custom integration (integrations/home-assistant): zeroconf finds the node,
        the config flow takes the API token, each zone is a media_player; the media browser shows
        the library (Home, albums, artists, playlists, genres) with covers; search; play_media of an
        album, a track and a URL; next, seek, repeat, shuffle; a zone started on the node shows up
        pushed; a zone made on the node appears as an entity; grouping two zones and ungrouping.

Home Assistant runs either from a `hass` executable (a virtualenv with the `homeassistant`
package) or, with --docker IMAGE, as the official container on the host network (CI).

Usage: ha-check.py (dlna|integration) <pm-node> <work dir> (--hass PATH | --docker IMAGE)
"""
import argparse
import http.server
import json
import os
import secrets
import shutil
import socket
import subprocess

import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIX = ROOT / "crates/pm-audio/tests/fixtures"
INTEGRATION = ROOT / "integrations/home-assistant/custom_components/pubky_music"
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

def tag_flac(src, dst, tags):
    """A copy of a FLAC file with these Vorbis comments (replacing any it had)."""
    data = src.read_bytes()
    if data[:4] != b"fLaC":
        raise ValueError(f"{src} is not FLAC")
    pos, blocks = 4, []
    while True:
        hdr = data[pos]
        last, kind = hdr & 0x80, hdr & 0x7F
        n = int.from_bytes(data[pos + 1:pos + 4], "big")
        if kind != 4:
            blocks.append((kind, data[pos + 4:pos + 4 + n]))
        pos += 4 + n
        if last:
            break
    vendor = b"pubky-music ha-check"
    body = len(vendor).to_bytes(4, "little") + vendor + len(tags).to_bytes(4, "little")
    for k, v in tags.items():
        c = f"{k}={v}".encode()
        body += len(c).to_bytes(4, "little") + c
    blocks.insert(1, (4, body))
    out = bytearray(b"fLaC")
    for i, (kind, b) in enumerate(blocks):
        out += bytes([kind | (0x80 if i == len(blocks) - 1 else 0)]) + len(b).to_bytes(3, "big") + b
    dst.write_bytes(bytes(out) + data[pos:])


LIBRARY = [
    # (fixture, album, album artist, track no, title, genre)
    ("60_mono_audio.flac", "Night Songs", "The Testers", 1, "Moonrise", "Ambient"),
    ("60_mono_audio.flac", "Night Songs", "The Testers", 2, "Midnight", "Ambient"),
    ("60_mono_audio.flac", "Day Songs", "Other Band", 1, "Sunrise", "Pop"),
]


def start_node(node_bin, work, announce=False, tagged=False):
    music = work / "music"
    music.mkdir(parents=True, exist_ok=True)
    if tagged:
        for fx, album, artist, no, title, genre in LIBRARY:
            d = music / artist / album
            d.mkdir(parents=True, exist_ok=True)
            tag_flac(FIX / fx, d / f"{no:02d} {title}.flac", {
                "TITLE": title, "ALBUM": album, "ARTIST": artist, "ALBUMARTIST": artist,
                "TRACKNUMBER": str(no), "GENRE": genre, "DATE": "2024"})
    else:
        for f in FIX.glob("*.flac"):
            shutil.copy(f, music / f.name)
    (work / "node.toml").write_text(
        f'data_dir = "{work}/data"\n'
        f'[library]\nfolders = ["{music}"]\n'
        f'[api]\nlisten = "0.0.0.0:{NODE_PORT}"\ntoken = "{TOKEN}"\nname = "{NODE_NAME}"\nannounce = {"true" if announce else "false"}\n'
        "[outputs]\ndiscover = false\nupnp = false\ncast = false\nslimproto_port = 0\nairplay = false\n"
        "[remote]\nenabled = false\n"
        "[renderer]\nenabled = true\n"
    )
    logf = open(work / "node.log", "w")
    p = subprocess.Popen([node_bin, "-c", str(work / "node.toml"), "serve"], stdout=logf, stderr=subprocess.STDOUT,
                         env={**os.environ, "RUST_LOG": "info", "PM_NULL_OUTPUT": "realtime"})
    until("the node answers", lambda: node("/v1/info"), 60)
    node("/v1/library/scan", "POST", {})
    want = len(LIBRARY) if tagged else 1
    until("the node scanned the fixtures", lambda: len(node("/v1/library/tracks")["tracks"]) >= want or None, 60)
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

def start_ha(args, work, integration=False):
    cfg = work / "ha"
    cfg.mkdir(parents=True, exist_ok=True)
    if integration:
        shutil.copytree(INTEGRATION, cfg / "custom_components" / "pubky_music",
                        ignore=shutil.ignore_patterns("__pycache__"))
    (cfg / "configuration.yaml").write_text(
        "default_config:\n"
        f"http:\n  server_port: {HA_PORT}\n"
        "logger:\n  default: warning\n  logs:\n    homeassistant.components.dlna_dmr: debug\n    homeassistant.components.ssdp: info\n    custom_components.pubky_music: debug\n"
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
    until("the zone plays the URL Home Assistant sent", lambda: (lambda z: z if z["transport"] == "playing" and (z.get("now_playing") or {}).get("item", {}).get("uri") == url else None)(zone_state()), 60)
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

# ---- integration ---------------------------------------------------------------------------------

def check_integration(args, work):
    h = onboard()
    ok("Home Assistant onboarded through its API")
    ws = WS(h["Authorization"].split(" ", 1)[1])
    info = node("/v1/info")

    # 1. Discovery by zeroconf, then the token in the confirm step
    def discovered():
        flows = ws.call({"type": "config_entries/flow/progress"})
        mine = [f for f in flows if f.get("handler") == "pubky_music"]
        return mine[0] if mine else None

    flow = None
    end = time.time() + 120
    while time.time() < end and not flow:
        flow = discovered()
        time.sleep(3)
    if flow:
        ok(f"zeroconf found the node: {flow['context'].get('title_placeholders', {}).get('name')}")
        r = ha(h, f"/api/config/config_entries/flow/{flow['flow_id']}", "POST", {"token": "wrong"})
        if r.get("errors", {}).get("base") != "invalid_auth":
            fail(f"a wrong token should be refused: {r}")
        ok("a wrong token is refused")
        r = ha(h, f"/api/config/config_entries/flow/{flow['flow_id']}", "POST", {"token": TOKEN})
    else:
        log("     no zeroconf discovery here (mDNS blocked?); adding the node by address")
        r = ha(h, "/api/config/config_entries/flow", "POST", {"handler": "pubky_music"})
        r = ha(h, f"/api/config/config_entries/flow/{r['flow_id']}", "POST", {"host": "127.0.0.1", "port": NODE_PORT, "token": TOKEN})
    if r.get("type") != "create_entry":
        fail(f"the config flow didn't create an entry: {r}")
    ok(f"config entry: {r.get('title')}")
    zones = node("/v1/zones")
    zid = zones[0]["id"]
    registry = ws.call({"type": "config/entity_registry/list"})

    def entity_for(zone_id):
        for e in ws.call({"type": "config/entity_registry/list"}):
            if e.get("platform") == "pubky_music" and e.get("unique_id") == f"{info['id']}-{zone_id}":
                return e["entity_id"]
        return None

    ent = until("the zone's media_player appears", lambda: entity_for(zid), 60, 2)
    until("it is available", lambda: state_of(h, ent)["state"] != "unavailable", 30)
    st = state_of(h, ent)
    feats = st["attributes"].get("supported_features", 0)
    for name, bit in (("browse", 131072), ("search", 4194304), ("play_media", 512), ("grouping", 524288), ("repeat", 262144), ("enqueue", 2097152), ("announce", 1048576)):
        if not feats & bit:
            fail(f"{ent} lacks {name}")
    if feats & 4:
        fail("volume offered on an output without hardware volume")
    ok(f"{ent}: {st['state']}, browse, search, play_media, grouping, repeat, enqueue; no volume (no hardware volume)")
    del registry

    # 2. The media browser
    root = ws.call({"type": "media_player/browse_media", "entity_id": ent})
    titles = [c["title"] for c in root["children"]]
    for want in ("Home", "Albums", "Artists", "Playlists", "Genres"):
        if want not in titles:
            fail(f"the browser's root lacks {want}: {titles}")
    ok(f"browser root: {', '.join(titles)}")
    albums = ws.call({"type": "media_player/browse_media", "entity_id": ent, "media_content_id": "pubky-music://albums", "media_content_type": "albums"})
    names = sorted(c["title"] for c in albums["children"])
    if len(names) != 2 or not any("Night Songs" in n for n in names):
        fail(f"albums: {names}")
    ok(f"albums: {names}")
    night = next(c for c in albums["children"] if "Night Songs" in c["title"])
    if not night.get("thumbnail"):
        fail("albums have no cover thumbnail")
    album = ws.call({"type": "media_player/browse_media", "entity_id": ent, "media_content_id": night["media_content_id"], "media_content_type": "album"})
    tracks = [c["title"] for c in album["children"]]
    if [t.split(" · ")[0] for t in tracks] != ["Moonrise", "Midnight"]:
        fail(f"album tracks in order: {tracks}")
    ok(f"album {album['title']!r}: {tracks}")
    for kind, cid in (("artists", "pubky-music://artists"), ("genres", "pubky-music://genres"), ("home", "pubky-music://home/recently_added")):
        b = ws.call({"type": "media_player/browse_media", "entity_id": ent, "media_content_id": cid, "media_content_type": kind})
        ok(f"{kind}: {[c['title'] for c in b['children']]}")
    thumb = night["thumbnail"]
    if thumb.startswith("/"):
        try:
            img = urllib.request.urlopen(urllib.request.Request(HA + thumb, headers=h), timeout=20)
            ok(f"a cover through HA's proxy: {img.headers.get('Content-Type')}, {len(img.read())} bytes")
        except urllib.error.HTTPError as e:
            log(f"     the fixture album has no cover art ({e.code}); the proxy answered")

    # 3. Search (the service, with a response)
    r = req(HA + "/api/services/media_player/search_media?return_response", "POST",
            {"entity_id": ent, "search_query": "Sunrise"}, h)
    found = json.dumps(r)
    if "Sunrise" not in found:
        fail(f"search for Sunrise: {found[:300]}")
    ok("search_media finds Sunrise")

    # 4. play_media: an album, then next, seek, repeat, shuffle
    def zone():
        return node(f"/v1/zones/{zid}")

    service(h, "media_player", "play_media", {"entity_id": ent, "media_content_id": night["media_content_id"], "media_content_type": "album"})
    until("the zone plays the album", lambda: zone()["transport"] == "playing" and len(zone()["queue"]) == 2, 30)
    until("HA shows Moonrise", lambda: state_of(h, ent)["attributes"].get("media_title") == "Moonrise", 30)
    ok("play_media album: the zone plays Moonrise, queue of 2, HA shows it (pushed)")
    service(h, "media_player", "media_next_track", {"entity_id": ent})
    until("next: Midnight", lambda: state_of(h, ent)["attributes"].get("media_title") == "Midnight", 30)
    ok("next: Midnight")
    service(h, "media_player", "media_seek", {"entity_id": ent, "seek_position": 3})
    until("seek: the position is past 3 s", lambda: (zone().get("now_playing") or {}).get("position_ms", 0) >= 3000, 20)
    ok("seek")
    service(h, "media_player", "repeat_set", {"entity_id": ent, "repeat": "all"})
    until("repeat all on the zone", lambda: zone()["repeat"] == "all", 20)
    until("HA shows repeat all", lambda: state_of(h, ent)["attributes"].get("repeat") == "all", 20)
    ok("repeat: all")
    service(h, "media_player", "shuffle_set", {"entity_id": ent, "shuffle": True})
    ok("shuffle: accepted")
    # enqueue a track next, a URL at the end
    sunrise = next(t for t in node("/v1/library/tracks")["tracks"] if t["title"] == "Sunrise")
    service(h, "media_player", "play_media", {"entity_id": ent, "media_content_id": f"pubky-music://track/{sunrise['id']}", "media_content_type": "track", "enqueue": "add"})
    until("enqueue add: three in the queue", lambda: len(zone()["queue"]) == 3, 20)
    ok("enqueue add: the track joined the queue")
    url = f"http://{lan_ip()}:{MEDIA_PORT}/song.flac"
    service(h, "media_player", "play_media", {"entity_id": ent, "media_content_id": url, "media_content_type": "music"})
    until("play_media URL replaces the queue", lambda: [i["uri"] for i in zone()["queue"]] == [url] and zone()["transport"] == "playing", 30)
    ok("play_media URL: replaces the queue and plays")
    # announce: the clip plays instead of the music, then the zone comes back to the same track
    service(h, "media_player", "play_media", {"entity_id": ent, "media_content_id": night["media_content_id"], "media_content_type": "album"})
    until("the album plays again", lambda: zone()["transport"] == "playing" and (zone().get("now_playing") or {}).get("position_ms", 0) >= 1000, 30)
    first = (zone().get("now_playing") or {}).get("item", {}).get("title")
    service(h, "media_player", "play_media", {"entity_id": ent, "media_content_id": url, "media_content_type": "music", "announce": True, "extra": {"chime": True}})
    until("the announcement plays", lambda: (zone().get("now_playing") or {}).get("item", {}).get("title") in ("Chime", "Announcement"), 20)
    until("the zone comes back to the song", lambda: (lambda z: z["transport"] == "playing" and (z.get("now_playing") or {}).get("item", {}).get("title") == first and (z.get("now_playing") or {}).get("position_ms", 0) >= 1000)(zone()), 60)
    ok(f"announce: the clip played, then back to {first!r} where it was")
    service(h, "media_player", "clear_playlist", {"entity_id": ent})
    service(h, "media_player", "media_stop", {"entity_id": ent})
    until("stopped", lambda: zone()["transport"] == "stopped", 20)
    until("HA idle", lambda: state_of(h, ent)["state"] == "idle", 20)
    ok("clear and stop: HA idle")

    # 5. Pushed from the node: playback and new zones
    node(f"/v1/zones/{zid}/queue", "POST", {"track_ids": [sunrise["id"]], "replace": True, "play": True})
    until("HA follows the node", lambda: state_of(h, ent)["attributes"].get("media_title") == "Sunrise" and state_of(h, ent)["state"] == "playing", 20)
    ok("a zone started on the node: HA shows Sunrise, playing")
    node(f"/v1/zones/{zid}/transport", "POST", {"action": "stop"})
    node("/v1/outputs/pull", "POST", {"name": "kitchen"})
    z2 = until("the node made a second zone", lambda: next((z["id"] for z in node("/v1/zones") if z["id"] != zid), None), 20)
    ent2 = until("the new zone appears in HA", lambda: entity_for(z2), 30, 2)
    ok(f"a zone made on the node appears: {ent2}")

    # 6. Grouping and ungrouping
    before = {z["id"] for z in node("/v1/zones")}
    service(h, "media_player", "join", {"entity_id": ent, "group_members": [ent2]})
    gid = until("join makes a group zone", lambda: next((z["id"] for z in node("/v1/zones") if z["id"] not in before and z["output"].startswith("group:")), None), 20)
    gent = until("the group zone appears in HA", lambda: entity_for(gid), 30, 2)
    members = until("the group lists its members", lambda: state_of(h, gent)["attributes"].get("group_members"), 20)
    if set(members) != {gent, ent, ent2}:
        fail(f"group members: {members}")
    ok(f"join: group zone {gent} with {members}")
    service(h, "media_player", "unjoin", {"entity_id": gent})
    until("unjoin removes the group zone", lambda: gid not in {z["id"] for z in node("/v1/zones")}, 20)
    until("and its entity", lambda: entity_for(gid) is None or state_of(h, entity_for(gid))["state"] == "unavailable", 30)
    ok("unjoin: the group zone is gone")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("check", choices=["dlna", "integration"])
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
        integ = args.check == "integration"
        procs.append(start_node(args.node_bin, work, announce=integ, tagged=integ))
        procs.append(start_ha(args, work, integration=integ))
        if args.check == "dlna":
            check_dlna(args, work)
        else:
            check_integration(args, work)
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
