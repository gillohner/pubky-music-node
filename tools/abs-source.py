#!/usr/bin/env python3
"""The Audiobookshelf source against a real Audiobookshelf (CI: ci.yml, audiobookshelf).

Runs Audiobookshelf's server from its source (a pinned release tag, built with
npm, no Docker) with a book library of two generated 30-second files, then
checks the node:
- signing in with a user and password makes the node its own API key, and only
  the key is kept;
- search, and the book with its chapters;
- the relayed stream is byte-identical to the file;
- playing resumes where Audiobookshelf says (as a phone left it), in the second
  file, and the node reports the position back;
- signing out deletes the node's key.

Usage: abs-source.py <pm-node> <audiobookshelf source dir, built> <ffmpeg> <work dir>
"""
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

node_bin, abs_src, ffmpeg, work = sys.argv[1], Path(sys.argv[2]), sys.argv[3], Path(sys.argv[4])
ABS = "http://127.0.0.1:13378"
API = "http://127.0.0.1:7817"
TOKEN = "ci-token"


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


def req(url, method="GET", body=None, headers=None, raw=False):
    r = urllib.request.Request(url, method=method, data=None if body is None else json.dumps(body).encode(),
                               headers=headers or {})
    if body is not None:
        r.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(r, timeout=30) as resp:
        data = resp.read()
        return data if raw else (json.loads(data) if data else None)


def api(method, path, body=None, raw=False):
    return req(API + path, method, body, {"Authorization": f"Bearer {TOKEN}"}, raw)


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
    book = work / "books" / "Test Author" / "A Test Book"
    book.mkdir(parents=True)
    for i in (1, 2):
        subprocess.run([ffmpeg, "-loglevel", "error", "-y", "-f", "lavfi", "-i", f"sine=frequency={200 * i}:duration=30",
                        "-ac", "1", "-ar", "44100", "-metadata", f"title=Chapter {i}", "-metadata", "artist=Test Author",
                        "-metadata", "album=A Test Book", "-metadata", f"track={i}", str(book / f"0{i}.mp3")], check=True)
    for d in ("config", "metadata", "data"):
        (work / d).mkdir()
    env = dict(os.environ, PORT="13378", HOST="127.0.0.1", CONFIG_PATH=str(work / "config"),
               METADATA_PATH=str(work / "metadata"), FFMPEG_PATH=ffmpeg,
               FFPROBE_PATH=str(Path(ffmpeg).with_name("ffprobe")), ROUTER_BASE_PATH="", SOURCE="local",
               NODE_ENV="production")
    procs = []
    try:
        procs.append(subprocess.Popen(["node", "dist-server/index.js"], cwd=abs_src, env=env,
                                      stdout=open(work / "abs.log", "w"), stderr=subprocess.STDOUT))
        st = until("Audiobookshelf", lambda: req(ABS + "/status"))
        log(f"Audiobookshelf {st['serverVersion']} is up")
        req(ABS + "/init", "POST", {"newRoot": {"username": "root", "password": "ci-pw"}}, raw=True)
        tok = req(ABS + "/login", "POST", {"username": "root", "password": "ci-pw"}, {"x-return-tokens": "true"})["user"]["accessToken"]
        admin = {"Authorization": f"Bearer {tok}"}
        lib = req(ABS + "/api/libraries", "POST", {"name": "Books", "folders": [{"fullPath": str(work / "books")}],
                                                   "mediaType": "book"}, admin)["id"]
        req(f"{ABS}/api/libraries/{lib}/scan", "POST", {}, admin, raw=True)
        item = until("the scan", lambda: req(f"{ABS}/api/libraries/{lib}/items", headers=admin)["results"])[0]["id"]
        log(f"Audiobookshelf scanned the book ({item})")

        (work / "node.toml").write_text(f"""data_dir = "{work}/data"
[library]
folders = []
[api]
listen = "127.0.0.1:7817"
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
                                      env=dict(os.environ, PM_NULL_OUTPUT="realtime"),
                                      stdout=open(work / "node.log", "w"), stderr=subprocess.STDOUT))
        until("the node", lambda: api("GET", "/v1/info"))
        bad = status_of(lambda: api("PUT", "/v1/sources/audiobookshelf/server",
                                    {"url": ABS, "user": "root", "password": "wrong"}))
        assert bad == 401, bad
        v = api("PUT", "/v1/sources/audiobookshelf/server", {"url": ABS, "user": "root", "password": "ci-pw"})
        assert v["connected"] and v["user"] == "root", v
        keys = req(ABS + "/api/api-keys", headers=admin)["apiKeys"]
        assert [k["name"] for k in keys] == ["Pubky Music node"], keys
        log(f"connected; the node made its own API key: {v}")

        hits = api("GET", "/v1/sources/audiobookshelf/search?q=test")["items"]
        assert hits and hits[0]["id"] == f"book:{item}", hits
        b = api("GET", f"/v1/sources/audiobookshelf/items/book:{item}")
        assert [t["title"] for t in b["tracks"]] == ["Chapter 1", "Chapter 2"], b
        log(f"book: {b['title']} by {b['creator']}, {[t['title'] for t in b['tracks']]}")
        ino = b["tracks"][0]["url"].rsplit("/", 1)[1]
        data = api("GET", f"/v1/sources/audiobookshelf/stream/{item}/{ino}", raw=True)
        assert data == (book / "01.mp3").read_bytes(), f"the relayed stream differs ({len(data)} bytes)"
        log(f"stream: byte-identical to the file ({len(data)} bytes)")

        # A phone left off 40 s in: that is 10 s into the second file
        req(f"{ABS}/api/me/progress/{item}", "PATCH", {"currentTime": 40, "duration": 60, "progress": 0.66}, admin, raw=True)
        started = api("POST", "/v1/sources/audiobookshelf/play", {"zone": "null-output", "item": f"book:{item}"})
        assert started["index"] == 1 and started["position_ms"] == 40000, started
        z = until("the zone to play", lambda: (lambda z: z if z["transport"] == "playing" and z["now_playing"]["position_ms"] > 0 else None)(
            api("GET", "/v1/zones/null-output")))
        pos = z["now_playing"]["position_ms"]
        assert z["now_playing"]["item"]["title"] == "Chapter 2" and 9000 <= pos <= 15000, z["now_playing"]
        log(f"resumed: {z['now_playing']['item']['title']} at {pos} ms")
        p = until("the node's position report", lambda: (lambda p: p if p["currentTime"] > 42 else None)(
            req(f"{ABS}/api/me/progress/{item}", headers=admin)), timeout=30)
        assert p["currentTime"] < 60 and 0.7 < p["progress"] < 1, p
        log(f"Audiobookshelf has the node's position: {p['currentTime']:.1f} s ({p['progress']:.2f})")
        api("POST", "/v1/zones/null-output/transport", {"action": "stop"})

        # Audiobookshelf's keys and tokens are JWTs
        assert "eyJ" not in (work / "node.log").read_text(), "a key or token reached the log"
        v = api("DELETE", "/v1/sources/audiobookshelf/server")
        assert not v["connected"], v
        assert req(ABS + "/api/api-keys", headers=admin)["apiKeys"] == [], "the node's key is still there"
        log("signed out: the node's key is deleted")
        log("PASS: the Audiobookshelf source works against a real Audiobookshelf")
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
