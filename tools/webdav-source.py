#!/usr/bin/env python3
"""A WebDAV share as a library root, against a real WebDAV server (CI: ci.yml, webdav).

Runs WsgiDAV (a maintained Python WebDAV server) on a folder of the test fixtures, in two
albums and with a space and an ampersand in a name, behind a user name and password. Then it
checks the node:
- a wrong password is refused;
- adding the share scans it (both songs, with their formats);
- a zone plays a song from the share to the end;
- the browser stream is byte-identical to the file on the share;
- a file deleted from the share is gone after a rescan;
- removing the share removes its songs.

Usage: webdav-source.py <pm-node> <wsgidav> <work dir>
"""
import json
import secrets
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

node_bin, wsgidav, work = sys.argv[1], sys.argv[2], Path(sys.argv[3])
API = "http://127.0.0.1:7818"
TOKEN = "ci-token"
DAV = "http://127.0.0.1:8098"
FIX = Path(__file__).resolve().parent.parent / "crates/pm-audio/tests/fixtures"
# Made up for each run: the test server lives as long as the test
DAV_USER = "ci-user"
DAV_PASS = secrets.token_hex(12)


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


def api(method, path, body=None, raw=False):
    r = urllib.request.Request(API + path, method=method, data=None if body is None else json.dumps(body).encode(),
                               headers={"Authorization": f"Bearer {TOKEN}"})
    if body is not None:
        r.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(r, timeout=60) as resp:
        data = resp.read()
        return data if raw else (json.loads(data) if data else None)


def status_of(fn):
    try:
        fn()
        return 200
    except urllib.error.HTTPError as e:
        return e.code


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
    share = work / "share"
    (share / "Queen & Co").mkdir(parents=True)
    (share / "Other").mkdir()
    shutil.copy(FIX / "60_mono_audio.flac", share / "Queen & Co" / "01 Mono Song.flac")
    shutil.copy(FIX / "14_wasted_bits.flac", share / "Other" / "02 Wasted.flac")
    (share / "notes.txt").write_text("not audio")
    (work / "data").mkdir()
    (work / "dav.yaml").write_text(f"""host: 127.0.0.1
port: 8098
provider_mapping:
  "/music": "{share}"
http_authenticator:
  domain_controller: null
  accept_basic: true
  accept_digest: false
  default_to_digest: false
simple_dc:
  user_mapping:
    "*":
      "{DAV_USER}":
        password: "{DAV_PASS}"
verbose: 1
""")
    (work / "node.toml").write_text(f"""data_dir = "{work}/data"
[library]
folders = []
[api]
listen = "127.0.0.1:7818"
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
    procs = []
    try:
        procs.append(subprocess.Popen([wsgidav, "--config", str(work / "dav.yaml")],
                                      stdout=open(work / "dav.log", "w"), stderr=subprocess.STDOUT))
        procs.append(subprocess.Popen([node_bin, "--config", str(work / "node.toml"), "serve"],
                                      env=dict(os.environ, PM_NULL_OUTPUT="realtime"),
                                      stdout=open(work / "node.log", "w"), stderr=subprocess.STDOUT))
        until("the node", lambda: api("GET", "/v1/info"))
        import socket
        until("WsgiDAV", lambda: socket.create_connection(("127.0.0.1", 8098), timeout=2).close() or True)
        url = DAV + "/music/"
        bad = status_of(lambda: api("POST", "/v1/library/webdav", {"url": url, "user": DAV_USER, "password": "wrong-" + DAV_PASS}))
        assert bad == 502, bad
        sh = api("POST", "/v1/library/webdav", {"url": url, "user": DAV_USER, "password": DAV_PASS, "name": "NAS"})
        assert DAV_PASS not in json.dumps(sh), sh
        log(f"share added: {sh}")
        shares = until("the scan", lambda: (lambda l: l if l and l[0]["tracks"] == 2 else None)(api("GET", "/v1/library/webdav")))
        log(f"scanned: {shares[0]['tracks']} songs, {shares[0]['bytes']} bytes")
        tracks = api("GET", "/v1/library/tracks")["tracks"]
        mono = [t for t in tracks if t["file"]["path"].endswith("01 Mono Song.flac")][0]
        assert mono["file"]["path"].startswith("webdav://") and mono["file"]["container"] == "flac", mono["file"]
        data = api("GET", f"/v1/library/tracks/{mono['id']}/file", raw=True)
        assert data == (share / "Queen & Co" / "01 Mono Song.flac").read_bytes(), f"the stream differs ({len(data)} bytes)"
        log(f"browser stream: byte-identical ({len(data)} bytes)")
        zone = api("GET", "/v1/zones")[0]["id"]
        api("POST", f"/v1/zones/{zone}/queue", {"track_ids": [mono["id"]], "replace": True, "play": True})
        until("the zone to start", lambda: api("GET", f"/v1/zones/{zone}")["transport"] == "playing", timeout=30)
        z = until("the zone to finish", lambda: (lambda z: z if z["transport"] == "stopped" else None)(api("GET", f"/v1/zones/{zone}")), timeout=60)
        assert z["error"] is None, z["error"]
        log("zone: played the song from the share to the end")
        (share / "Other" / "02 Wasted.flac").unlink()
        api("POST", f"/v1/library/webdav/{sh['id']}/scan")
        until("the rescan", lambda: api("GET", "/v1/library/webdav")[0]["tracks"] == 1)
        log("a file deleted from the share is gone after a rescan")
        assert DAV_PASS not in (work / "node.log").read_text(), "the password reached the log"
        gone = api("DELETE", f"/v1/library/webdav/{sh['id']}")
        assert gone["files"] >= 1, gone
        left = [t for t in api("GET", "/v1/library/tracks")["tracks"] if t["file"]["path"].startswith("webdav://")]
        assert not left, left
        log("share removed with its songs")
        log("PASS: a WebDAV share works as a library root")
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()


if __name__ == "__main__":
    main()
