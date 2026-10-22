#!/usr/bin/env python3
"""The Plex source against a real Plex Media Server (CI: ci.yml, plex).

Runs Plex's own Linux build (unpacked from the official .deb, no Docker) with a
music library of the test fixtures, then checks the node: connect by address and
token, the server's name, search, an album, a zone playing the original file,
and the relayed stream being byte-identical to the file on disk. The server
trusts 127.0.0.1 (`allowedNetworks`), as an unclaimed server has no accounts;
the sign-in with a plex.tv PIN is covered by the mock test in pm-api.

Usage: plex-source.py <pm-node> <unpacked plexmediaserver deb> <work dir>
"""
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

node_bin, pms_root, work = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
PMS = "http://127.0.0.1:32400"
API = "http://127.0.0.1:7816"
TOKEN = "ci-token"
FIXTURE = Path(__file__).resolve().parent.parent / "crates/pm-audio/tests/fixtures/60_mono_audio.flac"


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


def get(url, headers=None, method="GET", body=None, raw=False):
    req = urllib.request.Request(url, method=method, data=None if body is None else json.dumps(body).encode(),
                                 headers={"Accept": "application/json", **(headers or {})})
    if body is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=30) as r:
        data = r.read()
        return (data, r.headers.get("Content-Type")) if raw else (json.loads(data) if data else None)


def api(method, path, body=None, raw=False):
    return get(API + path, {"Authorization": f"Bearer {TOKEN}"}, method, body, raw)


def until(what, fn, timeout=90):
    end = time.time() + timeout
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
        except Exception:
            pass
        time.sleep(1)
    raise SystemExit(f"FAIL: timed out waiting for {what}")


def main():
    shutil.rmtree(work, ignore_errors=True)
    support = work / "pms" / "Plex Media Server"
    support.mkdir(parents=True)
    (work / "tmp").mkdir()
    (support / "Preferences.xml").write_text(
        '<?xml version="1.0" encoding="utf-8"?>\n<Preferences allowedNetworks="127.0.0.1/255.255.255.255" '
        'DlnaEnabled="0" GdmEnabled="0" FriendlyName="pm-ci" AcceptedEULA="1" '
        'PublishServerOnPlexOnlineKey="0" TelemetryOptOut="1"/>\n')
    music = work / "music" / "Fixtures"
    music.mkdir(parents=True)
    shutil.copy(FIXTURE, music / FIXTURE.name)
    home = pms_root / "usr/lib/plexmediaserver"
    env = dict(os.environ, PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR=str(work / "pms"),
               PLEX_MEDIA_SERVER_HOME=str(home), LD_LIBRARY_PATH=str(home / "lib"), TMPDIR=str(work / "tmp"))
    procs = []
    try:
        # Its own session, so stopping it also stops the helpers it starts (tuner, scanner)
        procs.append(subprocess.Popen([str(home / "Plex Media Server")], env=env, cwd=work, start_new_session=True,
                                      stdout=open(work / "pms.log", "w"), stderr=subprocess.STDOUT))
        ident = until("Plex to start", lambda: (lambda v: v if "startState" not in v["MediaContainer"] else None)(
            get(PMS + "/identity")))
        log(f"Plex Media Server {ident['MediaContainer']['version']} is up")
        q = urllib.parse.urlencode({"name": "Music", "type": "artist", "agent": "tv.plex.agents.music",
                                    "scanner": "Plex Music", "language": "en-US", "location": str(work / "music")})
        until("the music library", lambda: get(f"{PMS}/library/sections?{q}", method="POST"))
        n = until("Plex's scan", lambda: get(PMS + "/library/sections/1/all?type=10")["MediaContainer"].get("size"))
        log(f"Plex scanned {n} track(s)")

        (work / "data").mkdir()
        (work / "node.toml").write_text(f"""data_dir = "{work}/data"
[library]
folders = []
[api]
listen = "127.0.0.1:7816"
token = "{TOKEN}"
announce = false
[outputs]
discover = false
upnp = false
cast = false
airplay = false
slimproto_port = 0
[remote]
enabled = false
""")
        procs.append(subprocess.Popen([node_bin, "--config", str(work / "node.toml"), "serve"],
                                      stdout=open(work / "node.log", "w"), stderr=subprocess.STDOUT))
        until("the node", lambda: api("GET", "/v1/info"))
        v = api("PUT", "/v1/sources/plex/server", {"url": PMS, "token": "any-token"})
        assert v["connected"] and v["server_name"] == "pm-ci", v
        log(f"connected: {v}")
        info = [s for s in api("GET", "/v1/sources") if s["id"] == "plex"][0]
        assert info["status"] == "ready", info
        r = api("GET", "/v1/sources/plex/search?q=mono")
        tracks = [h for h in r["items"] if h["kind"] == "track"]
        assert tracks and tracks[0]["uri"].startswith("plex:track:"), r
        hit = tracks[0]
        log(f"search: {hit['title']} by {hit['creator']} ({hit['duration_ms']} ms) as {hit['uri']}")
        key = hit["uri"].rsplit(":", 1)[1]
        al = [h for h in r["items"] if h["kind"] == "album"]
        if al:
            item = api("GET", f"/v1/sources/plex/items/{al[0]['id']}")
            assert item["tracks"] and item["tracks"][0]["format"].startswith("FLAC"), item
            log(f"album: {item['title']}, {len(item['tracks'])} track(s), {item['tracks'][0]['format']}")
        data, ctype = api("GET", f"/v1/sources/plex/stream/{key}", raw=True)
        assert data == FIXTURE.read_bytes(), f"the relayed stream differs from the file ({len(data)} bytes, {ctype})"
        log(f"stream: byte-identical to the file ({len(data)} bytes, {ctype})")
        zone = api("GET", "/v1/zones")[0]["id"]
        api("POST", f"/v1/zones/{zone}/queue", {"items": [{"uri": hit["uri"], "title": hit["title"]}],
                                                  "replace": True, "play": True})
        until("the zone to start", lambda: api("GET", f"/v1/zones/{zone}")["transport"] == "playing", timeout=30)
        played = until("the zone to finish", lambda: (lambda z: z if z["transport"] == "stopped" else None)(
            api("GET", f"/v1/zones/{zone}")), timeout=60)
        assert played["error"] is None, played["error"]
        log("zone: played the original file to the end")
        assert "any-token" not in json.dumps([v, r]), "the token reached the API"
        assert "any-token" not in (work / "node.log").read_text(), "the token reached the log"
        log("PASS: the Plex source works against a real Plex Media Server")
    finally:
        for p in procs:
            p.terminate()
        try:
            os.killpg(procs[0].pid, 15)
        except (IndexError, ProcessLookupError):
            pass
        for p in procs:
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()


if __name__ == "__main__":
    main()
