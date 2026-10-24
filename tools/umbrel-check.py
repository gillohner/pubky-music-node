#!/usr/bin/env python3
"""The Umbrel app (deploy/umbrel/pubky-music-node) as umbrelOS would run it (CI: ci.yml, bundle).

The package is copied the way umbrelOS installs it (placeholders stripped) and started with
Umbrel's variables (APP_DATA_DIR, APP_SEED, APP_PASSWORD, UMBREL_ROOT); only the node's image is
swapped for the one built from this commit. Checks:
- the node (host network, port 7788, token APP_PASSWORD, which umbrelOS shows) finds its bundled
  homeserver (admin and database password APP_SEED, never shown);
- the homeserver's plain HTTP (6286) and admin API (6288) answer on 127.0.0.1 only, Pubky TLS
  (6287) and the node (7788) on the network;
- Settings › Remote access works: the tunnel folder is writable, the token is saved 0600 and
  cloudflared (running as the node's uid) reads it, and a saved tunnel asks for an Umbrel app
  restart in words (no docker command);
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
        headers={"Authorization": f"Bearer {password}", "Content-Type": "application/json"},
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
assert seed not in json.dumps(st)
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
# 0600 and owned by the node's uid (1000), so the runner (another uid) reads it as root
stored = subprocess.run(["sudo", "cat", str(saved)], capture_output=True, text=True, check=True).stdout
assert stored.strip() == token, "the token is in the folder cloudflared reads"
assert saved.stat().st_mode & 0o777 == 0o600, f"the token's mode is {oct(saved.stat().st_mode & 0o777)}, not 0600"
# cloudflared runs as the node's uid and still reads the 0600 token: "Starting tunnel" names the
# tunnel ID from the token; "Failed to read token file" would mean it can't
since = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
subprocess.run(["docker", "compose", "-p", project, "restart", "cloudflared"], check=True, capture_output=True)


def cloudflared_log():
    out = subprocess.run(["docker", "compose", "-p", project, "logs", "--no-color", "--since", since, "cloudflared"],
                         capture_output=True, text=True)
    text = out.stdout + out.stderr
    assert "Failed to read token file" not in text, "cloudflared can't read the token the node saved"
    return "tunnelID=00000000-0000-4000-8000-000000000000" in text


until("cloudflared to start with the saved token", cloudflared_log, timeout=90)
t = api("/v1/remote/tunnel")
assert t["configured"] and t["restart_needed"], t
assert t["restart_command"] is None and "Umbrel" in (t["restart_hint"] or ""), t
log("remote access: the token (0600) reaches cloudflared, which reads it; the restart is asked for in Umbrel's words")
api("/v1/remote/tunnel", "DELETE")

logs = subprocess.run(["docker", "compose", "-p", project, "logs", "--no-color"], capture_output=True, text=True).stdout
for name, secret in (("APP_SEED", seed), ("APP_PASSWORD", password), ("the tunnel token", token)):
    assert secret not in logs, f"{name} appears in a container log"
log("no secret in any container log")
log("PASS: the Umbrel app is up, and the node manages its own homeserver")
