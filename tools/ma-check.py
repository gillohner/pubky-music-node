#!/usr/bin/env python3
"""Music Assistant against a real node (STEWARD-COMPAT-HA-MA.md §1 and §4; CI: compat.yml).

Starts a node with a small tagged library (zones as DLNA renderers, OpenSubsonic API on) and the
official Music Assistant container on the host network, then through MA's own API:
- creates MA's first user (`POST /setup`) and uses its token;
- adds the node as an **OpenSubsonic** music source (the provider's setup flow) and finds a track
  of the node's library with `music/search`;
- adds MA's **DLNA** player provider, which discovers the zone's renderer;
- plays the track to that player (`player_queues/play_media`): the node's zone plays MA's stream of
  the node's own track, and MA's player shows it playing.

Usage: ma-check.py <pm-node> <work dir> <image>   (e.g. ghcr.io/music-assistant/server:2.10.7)
"""
import json
import os
import secrets
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIX = ROOT / "crates/pm-audio/tests/fixtures"
NODE_PORT = 7822
MA = "http://127.0.0.1:8095"
TOKEN = "ci-token"
SUBSONIC_USER = "ci"
NODE = f"http://127.0.0.1:{NODE_PORT}"
CONTAINER = "pm-ma-check"


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


def fail(m):
    log(f"FAIL {m}")
    raise SystemExit(1)


def ok(m):
    log(f"ok   {m}")


def req(url, method="GET", body=None, headers=None, timeout=60):
    h = dict(headers or {})
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        h["Content-Type"] = "application/json"
    r = urllib.request.Request(url, method=method, data=data, headers=h)
    with urllib.request.urlopen(r, timeout=timeout) as resp:
        raw = resp.read()
        return json.loads(raw) if raw else None


def node(path, method="GET", body=None):
    return req(NODE + path, method, body, {"Authorization": f"Bearer {TOKEN}"})


def until(what, fn, timeout=90, every=2.0):
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


def tag_flac(src, dst, tags):
    """A copy of a FLAC file with these Vorbis comments (as tools/ha-check.py)."""
    data = src.read_bytes()
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
    vendor = b"pubky-music ma-check"
    body = len(vendor).to_bytes(4, "little") + vendor + len(tags).to_bytes(4, "little")
    for k, v in tags.items():
        c = f"{k}={v}".encode()
        body += len(c).to_bytes(4, "little") + c
    blocks.insert(1, (4, body))
    out = bytearray(b"fLaC")
    for i, (kind, b) in enumerate(blocks):
        out += bytes([kind | (0x80 if i == len(blocks) - 1 else 0)]) + len(b).to_bytes(3, "big") + b
    dst.write_bytes(bytes(out) + data[pos:])


def start_node(node_bin, work):
    music = work / "music" / "The Testers" / "Night Songs"
    music.mkdir(parents=True, exist_ok=True)
    for no, title in ((1, "Moonrise"), (2, "Midnight")):
        tag_flac(FIX / "60_mono_audio.flac", music / f"{no:02d} {title}.flac", {
            "TITLE": title, "ALBUM": "Night Songs", "ARTIST": "The Testers", "ALBUMARTIST": "The Testers",
            "TRACKNUMBER": str(no), "DATE": "2024", "GENRE": "Ambient"})
    (work / "node.toml").write_text(
        f'data_dir = "{work}/data"\n'
        f'[library]\nfolders = ["{work}/music"]\n'
        f'[api]\nlisten = "0.0.0.0:{NODE_PORT}"\ntoken = "{TOKEN}"\nname = "PM MA check"\n'
        f'subsonic_user = "{SUBSONIC_USER}"\nannounce = false\n'
        "[outputs]\ndiscover = false\nupnp = false\ncast = false\nslimproto_port = 0\nairplay = false\n"
        "[remote]\nenabled = false\n[renderer]\nenabled = true\n"
    )
    logf = open(work / "node.log", "w")
    p = subprocess.Popen([node_bin, "-c", str(work / "node.toml"), "serve"], stdout=logf, stderr=subprocess.STDOUT,
                         env={**os.environ, "RUST_LOG": "info", "PM_NULL_OUTPUT": "realtime"})
    until("the node answers", lambda: node("/v1/info"), 60, 1)
    node("/v1/library/scan", "POST", {})
    until("the node scanned the library", lambda: len(node("/v1/library/tracks")["tracks"]) >= 2 or None, 60, 1)
    return p


class MusicAssistant:
    def __init__(self, image, work):
        data = work / "ma-data"
        data.mkdir(parents=True, exist_ok=True)
        subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True)
        self.log = open(work / "ma.log", "w")
        subprocess.run(["docker", "run", "-d", "--name", CONTAINER, "--network", "host", "-v", f"{data}:/data", image],
                       check=True, capture_output=True)
        self.logs = subprocess.Popen(["docker", "logs", "-f", CONTAINER], stdout=self.log, stderr=subprocess.STDOUT)
        until("Music Assistant answers", lambda: req(MA + "/info", timeout=5), 300, 3)
        info = req(MA + "/info")
        ok(f"Music Assistant {info.get('server_version')} is up")
        password = secrets.token_hex(12)
        r = req(MA + "/setup", "POST", {"username": "ci", "password": password, "device_name": "ci"})
        if not r or not r.get("token"):
            fail(f"MA's first-user setup gave no token: {r}")
        self.token = r["token"]
        ok("MA's first user created, token in hand")
        try:
            self.api("config/onboard_complete")
        except urllib.error.HTTPError:
            pass

    def api(self, command, **args):
        try:
            return req(MA + "/api", "POST", {"command": command, "args": args, "message_id": secrets.token_hex(4)},
                       {"Authorization": f"Bearer {self.token}"}, timeout=120)
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace")[:400]
            raise RuntimeError(f"MA {command}: {e.code} {detail}") from None

    def setup_provider(self, domain, values=None):
        """A provider through its setup flow: forms are answered with `values`."""
        step = self.api("config/providers/setup", provider_domain=domain)
        for _ in range(6):
            kind = step.get("type")
            if kind in ("finish", "FINISH"):
                return step.get("result") or {}
            if kind in ("abort", "ABORT"):
                fail(f"MA's {domain} setup aborted: {step.get('reason') or step}")
            if kind in ("form", "FORM"):
                if step.get("errors"):
                    fail(f"MA's {domain} setup refused the values: {step['errors']}")
                keys = [e["key"] for e in step.get("entries", [])]
                step = self.api("config/flows/submit", flow_id=step["flow_id"],
                                values={k: v for k, v in (values or {}).items() if k in keys})
                continue
            if kind in ("progress", "PROGRESS"):
                time.sleep(2)
                step = self.api("config/flows/get", flow_id=step["flow_id"])
                continue
            fail(f"MA's {domain} setup: unexpected step {step}")
        fail(f"MA's {domain} setup did not finish: {step}")

    def stop(self):
        subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True)
        self.logs.terminate()


def lan_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))
        return s.getsockname()[0]
    except OSError:
        return "127.0.0.1"
    finally:
        s.close()


def main():
    node_bin, work, image = sys.argv[1], Path(sys.argv[2]).resolve(), sys.argv[3]
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    procs, ma = [], None
    try:
        procs.append(start_node(node_bin, work))
        ma = MusicAssistant(image, work)

        # 1. The node as an OpenSubsonic source
        r = ma.setup_provider("opensubsonic", {
            "baseURL": f"http://{lan_ip()}", "port": NODE_PORT, "path": "",
            "username": SUBSONIC_USER, "password": TOKEN,
        })
        instance = r.get("instance_id")
        ok(f"OpenSubsonic source added ({instance})")

        def found():
            res = ma.api("music/search", search_query="Moonrise", media_types=["track"], limit=10)
            tracks = [t for t in (res or {}).get("tracks", []) if t.get("name") == "Moonrise"]
            return tracks[0] if tracks else None

        track = until("MA finds the node's track Moonrise", found, 120, 3)
        ok(f"music/search: {track['name']} by {', '.join(a.get('name', '') for a in track.get('artists', []))} ({track['uri']})")

        # 2. The zone as a DLNA player
        ma.setup_provider("dlna")
        ok("DLNA player provider added")
        zone = node("/v1/zones")[0]

        def player():
            for p in ma.api("players/all") or []:
                name = p.get("display_name") or p.get("name") or ""
                if p.get("provider", "").startswith("dlna") and zone["name"] in name and p.get("available", True):
                    return p
            return None

        pl = until("MA's DLNA provider finds the zone's renderer", player, 180, 3)
        ok(f"player: {pl.get('display_name') or pl.get('name')} ({pl['player_id']})")

        # 3. Play the node's track to the node's zone, through MA
        ma.api("player_queues/play_media", queue_id=pl["player_id"], media=track["uri"])
        z = until("the zone plays what MA sent", lambda: (lambda z: z if z["transport"] == "playing" else None)(node(f"/v1/zones/{zone['id']}")), 90, 2)
        uri = ((z.get("now_playing") or {}).get("item") or {}).get("uri", "")
        ok(f"the zone plays MA's stream ({uri[:80]})")
        until("the position moves", lambda: (node(f"/v1/zones/{zone['id']}").get("now_playing") or {}).get("position_ms", 0) > 1500, 30, 1)
        ok("the position moves")
        st = until("MA shows its player playing", lambda: (lambda p: p if p and p.get("playback_state", p.get("state")) == "playing" else None)(ma.api("players/get", player_id=pl["player_id"])), 30, 2)
        ok(f"MA's player: {st.get('playback_state', st.get('state'))}")
        ma.api("players/cmd/stop", player_id=pl["player_id"])
        until("the zone stops when MA stops", lambda: node(f"/v1/zones/{zone['id']}")["transport"] == "stopped", 30, 1)
        ok("stop from MA: the zone stops")
        log("all Music Assistant checks pass")
    finally:
        if ma:
            ma.stop()
        for p in procs:
            p.terminate()
            try:
                p.wait(20)
            except subprocess.TimeoutExpired:
                p.kill()


if __name__ == "__main__":
    main()
