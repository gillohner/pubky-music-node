#!/usr/bin/env python3
"""How far apart two real squeezelite players play in one node group, measured
on ALSA (CI: ci.yml, alsa-loopback, inside the KVM guest with snd-aloop).

Both players use the ALSA output, as on a Pi: player A plays into the left
channel and player B into the right of one dmix on the loopback card, and one
`arecord` captures both. The track is a click every second; the distance
between a click on the left and on the right is how far apart the players
are, exactly, with no clock arithmetic. Both share the card's clock, so this
checks the synced start and that drift correction leaves in-step players
alone; drift between different clocks is covered by the unit tests.

Usage: slim-sync-alsa.py <pm-node> <work dir> [--seconds N] [--max-ms M]
"""
import argparse
import json
import os
import re
import struct
import subprocess
import time
import urllib.request
import wave
from pathlib import Path

ap = argparse.ArgumentParser()
ap.add_argument("node")
ap.add_argument("work", type=Path)
ap.add_argument("--seconds", type=int, default=30)
ap.add_argument("--max-ms", type=float, default=5.0)
ap.add_argument("--out", type=Path, help="copy the logs and the capture here at the end (a shared folder)")
a = ap.parse_args()
work = a.work
API = "http://127.0.0.1:7815"
SLIM = 3486
RATE = 44100
MACS = ["02:00:00:00:01:0a", "02:00:00:00:01:0b"]


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


def api(method, path, body=None):
    req = urllib.request.Request(API + path, data=json.dumps(body).encode() if body is not None else None,
                                 method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        raw = r.read()
    return json.loads(raw) if raw else None


def until(what, fn, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
        except Exception:  # noqa: BLE001
            pass
        time.sleep(0.5)
    raise SystemExit(f"FAIL: timed out waiting for {what}")


def main():
    work.mkdir(parents=True, exist_ok=True)
    cards = subprocess.run(["aplay", "-l"], capture_output=True, text=True).stdout
    card = re.search(r"^card (\d+): Loopback", cards, re.M)
    if not card:
        raise SystemExit("no Loopback card: modprobe snd-aloop")
    hw = card.group(1)
    # A to the left, B to the right, of one dmix on the loopback's playback side.
    (work / "asoundrc").write_text(f"""
pcm.mixloop {{
    type dmix
    ipc_key 4711
    ipc_perm 0666
    slave {{ pcm "hw:{hw},0,0" rate {RATE} format S16_LE channels 2 period_size 256 buffer_size 4096 }}
}}
pcm.player_a {{ type plug slave.pcm {{ type route slave {{ pcm "mixloop" channels 2 }} ttable.0.0 1 }} }}
pcm.player_b {{ type plug slave.pcm {{ type route slave {{ pcm "mixloop" channels 2 }} ttable.0.1 1 }} }}
""")
    env = dict(os.environ, RUST_LOG="info,pm_zones::outputs::group=debug")
    home = work / "home"
    home.mkdir(exist_ok=True)
    (home / ".asoundrc").write_text((work / "asoundrc").read_text())
    env["HOME"] = str(home)

    music = work / "music"
    music.mkdir(exist_ok=True)
    frames = bytearray(RATE * 4 * a.seconds)
    for k in range(a.seconds):
        struct.pack_into("<hh", frames, k * RATE * 4, 12000, 12000)
    with wave.open(str(music / "clicks.wav"), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(bytes(frames))
    (work / "node.toml").write_text(f"""data_dir = "{work}/data"
[library]
folders = ["{music}"]
[api]
listen = "127.0.0.1:7815"
announce = false
[outputs]
discover = false
upnp = false
cast = false
airplay = false
slimproto_port = {SLIM}
[remote]
enabled = false
""")
    procs = []
    try:
        cap = work / "capture.raw"
        rec = subprocess.Popen(["arecord", "-q", "-D", f"hw:{hw},1,0", "-f", "S16_LE", "-r", str(RATE), "-c", "2",
                                "-t", "raw", str(cap)], env=env)
        procs.append(rec)
        node = subprocess.Popen([a.node, "--config", str(work / "node.toml"), "serve"], env=env,
                                stdout=open(work / "node.log", "w"), stderr=subprocess.STDOUT)
        procs.append(node)
        until("the node", lambda: api("GET", "/v1/info"))
        api("POST", "/v1/library/scan", {})
        track = until("the scan", lambda: api("GET", "/v1/library/tracks")["tracks"])[0]["id"]
        for i, (mac, pcm) in enumerate(zip(MACS, ["player_a", "player_b"])):
            procs.append(subprocess.Popen(
                ["squeezelite", "-s", f"127.0.0.1:{SLIM}", "-o", pcm, "-a", "80:4:16:0", "-r", str(RATE),
                 "-n", pcm, "-m", mac, "-d", "slimproto=info", "-d", "output=info"],
                env=env, stdout=subprocess.DEVNULL, stderr=open(work / f"squeezelite-{i}.log", "w")))
        outs = until("both players", lambda: (lambda o: o if len(o) == 2 else None)(
            sorted(o["id"] for o in api("GET", "/v1/outputs") if o["id"].startswith("slim:"))))
        zone = api("POST", "/v1/zones", {"name": "Both", "outputs": outs})
        time.sleep(2)
        api("POST", f"/v1/zones/{zone['id']}/queue", {"track_ids": [track], "replace": True, "play": True})
        # What the group measures (its drift estimate for B, once a second), next to the truth.
        measured = []
        end = time.time() + a.seconds + 30
        while time.time() < end:
            z = api("GET", f"/v1/zones/{zone['id']}")
            if z["transport"] == "stopped" and measured:
                break
            members = (z.get("now_playing") or {}).get("members") or []
            if len(members) == 2:
                measured.append(members[1]["drift_ms"])
            time.sleep(1)
        time.sleep(1)
    finally:
        for p in procs:
            if p.poll() is None:
                p.terminate()
    data = (work / "capture.raw").read_bytes()
    left, right = [], []
    for i in range(0, len(data) - 3, 4):
        l, r = struct.unpack_from("<hh", data, i)
        n = i // 4
        if abs(l) > 6000 and (not left or n - left[-1] > RATE // 2):
            left.append(n)
        if abs(r) > 6000 and (not right or n - right[-1] > RATE // 2):
            right.append(n)
    if len(left) < a.seconds - 1 or len(right) < a.seconds - 1:
        raise SystemExit(f"FAIL: heard {len(left)} clicks from A and {len(right)} from B, of {a.seconds}")
    offs = [(b - x) * 1000 / RATE for x, b in zip(left, right)]
    log("B - A at each click (ms): " + " ".join(f"{o:+.1f}" for o in offs))
    worst = max(abs(o) for o in offs)
    log(f"start {offs[0]:+.2f} ms, end {offs[-1]:+.2f} ms, worst {worst:.2f} ms over {len(offs)} clicks")
    log("the group's own estimate of B - A, each second (ms): " + " ".join(f"{m:+d}" for m in measured))
    nudges = sum(1 for line in open(work / "node.log") if "group: nudged" in line)
    log(f"nudges: {nudges}")
    if worst > a.max_ms:
        raise SystemExit(f"FAIL: the players were {worst:.2f} ms apart (limit {a.max_ms} ms)")
    log(f"PASS: two squeezelite players in one group within {a.max_ms} ms at every click")


def keep_logs():
    if not a.out:
        return
    import shutil
    a.out.mkdir(parents=True, exist_ok=True)
    for f in list(work.glob("*.log")) + [work / "capture.raw", work / "asoundrc"]:
        if f.exists():
            shutil.copy(f, a.out / f.name)


if __name__ == "__main__":
    try:
        main()
    finally:
        keep_logs()
