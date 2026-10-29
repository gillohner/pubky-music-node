#!/usr/bin/env python3
"""backup.sh and restore.sh on the bundled install (CI: ci.yml, bundle).

  bundle-backup-check.py seed <bundle dir>                 a playlist, a favourite, a contact
  bundle-backup-check.py snap <bundle dir> <out.json>      what a restore must bring back
  bundle-backup-check.py compare <bundle dir> <before.json>

The snapshot holds what the owner sees (playlists, favourites, history, library counts, settings)
and the homeserver as the node sees it (key, version, users). Never a token or a password.
"""
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

mode, bundle = sys.argv[1], Path(sys.argv[2])
env = dict(line.split("=", 1) for line in (bundle / ".env").read_text().splitlines() if "=" in line)
API = "http://127.0.0.1:7788"
SONG = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"


def api(path, method="GET", body=None, token=True):
    headers = {"content-type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {env['PM_TOKEN']}"
    data = None if body is None else json.dumps(body).encode()
    r = urllib.request.Request(API + path, data=data, method=method, headers=headers)
    with urllib.request.urlopen(r, timeout=30) as resp:
        return json.loads(resp.read() or b"null")


def until(what, f, secs=180):
    end = time.time() + secs
    while True:
        try:
            v = f()
            if v:
                return v
        except Exception as e:  # noqa: BLE001 - keep trying until the deadline
            err = e
        else:
            err = None
        if time.time() > end:
            sys.exit(f"{what}: not ready after {secs} s ({err})")
        time.sleep(2)


def snap():
    until("the node", lambda: not api("/v1/info")["scanning"])
    hs = until("the bundled homeserver", lambda: (lambda v: v if v["reachable"] else None)(api("/v1/homeserver/bundled")))
    stats = api("/v1/library/stats")
    return {
        "name": api("/v1/info")["name"],
        "library": {k: stats[k] for k in ("tracks", "albums", "files")},
        "folders": api("/v1/library/folders"),
        "playlists": sorted((p["name"], p.get("description")) for p in api("/v1/playlists")),
        "favourites": sorted(api("/v1/favourites")["uris"]),
        "history": sorted(p["track"]["id"] for p in api("/v1/library/history?limit=1000")["plays"]),
        "contact": api("/v1/node/settings").get("contact"),
        "homeserver": {k: hs.get(k) for k in ("public_key", "version", "users")},
    }


if mode == "seed":
    until("the node", lambda: api("/v1/info"))
    api("/v1/node/settings", "PUT", {"contact": "https://example.org/backup-check"})
    api("/v1/playlists", "POST", {"name": "Backup check", "description": "kept across a restore", "uris": [SONG]})
    api("/v1/favourites", "PUT", {"uri": SONG})
    print("seeded a playlist, a favourite and the contact")
elif mode == "snap":
    s = snap()
    Path(sys.argv[3]).write_text(json.dumps(s, indent=1))
    print(json.dumps(s))
elif mode == "compare":
    before = json.loads(Path(sys.argv[3]).read_text())
    after = snap()
    # The token from .env still opens the node, and nothing opens it without one
    try:
        api("/v1/info", token=False)
        sys.exit("the restored node answers without a token")
    except urllib.error.HTTPError as e:
        assert e.code == 401, e.code
    bad = {k: (before[k], after.get(k)) for k in before if before[k] != after.get(k)}
    if bad:
        sys.exit(f"the restore differs: {json.dumps(bad)}")
    assert before["homeserver"]["users"], "no homeserver account was backed up"
    print(f"restored: {json.dumps(after)}")
else:
    sys.exit(__doc__)
