#!/usr/bin/env python3
"""The bundled install (deploy/bundle) as it comes up with `docker compose up` (CI: ci.yml, bundle).

Checks, against the real pubky-homeserver from the pinned image:
- the node finds its homeserver through the admin API (version, public key);
- the admin API and the homeserver's plain HTTP port answer on 127.0.0.1 only, while Pubky TLS
  (6287) and the node (7788) are on the network;
- the generated passwords appear in no container's log.

Usage: bundle-check.py <deploy/bundle dir>
"""
import json
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

bundle = Path(sys.argv[1])
env = dict(line.split("=", 1) for line in (bundle / ".env").read_text().splitlines() if "=" in line)
API = "http://127.0.0.1:7788"


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


def api(path):
    r = urllib.request.Request(API + path, headers={"Authorization": f"Bearer {env['PM_TOKEN']}"})
    with urllib.request.urlopen(r, timeout=15) as resp:
        return json.loads(resp.read())


def until(what, fn, timeout=180):
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
log(f"node → homeserver: {st['version']}, key {st['public_key']}, {st['users']} users")
assert "ADMIN_PASSWORD" not in json.dumps(st) and env["ADMIN_PASSWORD"] not in json.dumps(st)

ip = lan_ip()
for port, public in ((6287, True), (7788, True), (6286, False), (6288, False)):
    assert is_open("127.0.0.1", port), f"{port} is not listening at all"
    assert is_open(ip, port) == public, f"port {port} on {ip}: expected {'open' if public else 'closed'}"
log(f"exposure on {ip}: 6287 and 7788 open; 6286 and 6288 on 127.0.0.1 only")

logs = subprocess.run(["docker", "compose", "logs", "--no-color"], cwd=bundle, capture_output=True, text=True).stdout
for k in ("ADMIN_PASSWORD", "DB_PASSWORD", "PM_TOKEN"):
    assert env[k] not in logs, f"{k} appears in a container log"
log("no generated password in any container log")
log("PASS: the bundle is up, and the node manages its own homeserver")
