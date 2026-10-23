#!/usr/bin/env python3
"""The Umbrel app (deploy/umbrel/pubky-music-node) as umbrelOS would run it (CI: ci.yml, bundle).

The package is copied the way umbrelOS installs it (placeholders stripped) and started with
Umbrel's variables (APP_DATA_DIR, APP_SEED, APP_PASSWORD, UMBREL_ROOT); only the node's image is
swapped for the one built from this commit. Checks:
- the node (host network, port 7788, token APP_SEED) finds its bundled homeserver;
- the homeserver's plain HTTP (6286) and admin API (6288) answer on 127.0.0.1 only, Pubky TLS
  (6287) and the node (7788) on the network;
- Settings › Remote access works: the tunnel folder is writable, and a saved tunnel asks for an
  Umbrel app restart in words (no docker command);
- APP_SEED and APP_PASSWORD appear in no container's log.

Usage: umbrel-check.py <compose project name> <APP_SEED> <APP_PASSWORD> <APP_DATA_DIR>
"""
import base64
import json
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

project, seed, password, data_dir = sys.argv[1:5]
API = "http://127.0.0.1:7788"


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


def api(path, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(
        API + path,
        data=data,
        method=method,
        headers={"Authorization": f"Bearer {seed}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(r, timeout=15) as resp:
        raw = resp.read()
        return json.loads(raw) if raw else {}


def until(what, fn, timeout=240):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
        except Exception as e:
            last = e
        time.sleep(2)
    raise SystemExit(f"FAIL: timed out waiting for {what} ({last})")


def is_open(host, port):
    with socket.socket() as s:
        s.settimeout(2)
        return s.connect_ex((host, port)) == 0


def lan_ip():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.connect(("1.1.1.1", 80))
        return s.getsockname()[0]


until("the node", lambda: api("/v1/info"))
st = until("the bundled homeserver", lambda: (lambda v: v if v["reachable"] else None)(api("/v1/homeserver/bundled")))
assert st["version"] and st["public_key"], st
assert password not in json.dumps(st)
log(f"node → homeserver: {st['version']}, key {st['public_key']}")

ip = lan_ip()
for port, public in ((6287, True), (7788, True), (6286, False), (6288, False)):
    assert is_open("127.0.0.1", port), f"{port} is not listening at all"
    assert is_open(ip, port) == public, f"port {port} on {ip}: expected {'open' if public else 'closed'}"
log(f"exposure on {ip}: 6287 and 7788 open; 6286 and 6288 on 127.0.0.1 only")

t = api("/v1/remote/tunnel")
assert t["available"] and not t["configured"], t
token = base64.b64encode(json.dumps({"a": "ci", "t": "00000000-0000-4000-8000-000000000000", "s": "c2VjcmV0"}).encode()).decode()
api("/v1/remote/tunnel", "PUT", {"token": token, "homeserver_domain": "pubky.example.org"})
saved = Path(data_dir) / "data" / "cloudflare" / "token"
assert saved.read_text().strip() == token, "the token is in the folder cloudflared reads"
t = api("/v1/remote/tunnel")
assert t["configured"] and t["restart_needed"], t
assert t["restart_command"] is None and "Umbrel" in (t["restart_hint"] or ""), t
log("remote access: the token reaches cloudflared's folder; the restart is asked for in Umbrel's words")
api("/v1/remote/tunnel", "DELETE")

logs = subprocess.run(["docker", "compose", "-p", project, "logs", "--no-color"], capture_output=True, text=True).stdout
for name, secret in (("APP_SEED", seed), ("APP_PASSWORD", password), ("the tunnel token", token)):
    assert secret not in logs, f"{name} appears in a container log"
log("no secret in any container log")
log("PASS: the Umbrel app is up, and the node manages its own homeserver")
