#!/usr/bin/env python3
"""Slimproto against real squeezelite (CI: ci.yml, slimproto).

squeezelite writes its output to stdout (`-o -`); a pacer reads it at real
time (which gives squeezelite its clock) and keeps it from the first
non-silent byte. Checks:
- 16/44.1 and 24/96 noise come out bit-exact from the first sample (no WAV
  header played as a click), then silence;
- the zone's position follows real time on a longer track;
- pause silences the player within a second, and play resumes it;
- the same player connecting again (every check starts a new squeezelite with
  the same MAC) keeps its one zone.

Usage: slim-squeezelite.py <pm-node> <work dir>   (needs squeezelite)
"""
import json
import math
import random
import struct
import subprocess
import sys
import time
import urllib.request
import wave
from pathlib import Path

node_bin, work = sys.argv[1], Path(sys.argv[2])
API = "http://127.0.0.1:7813"
SLIM = 3483
MAC = "02:00:00:00:00:01"
ZONE = "squeezelite-" + MAC.replace(":", "-")
PACER = r'''
import sys, time
rate, fb, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
inp = sys.stdin.buffer; chunk = rate * fb // 100; started = False; t0 = time.monotonic(); n = 0
with open(out, "wb") as f:
    while True:
        b = inp.read(chunk)
        if not b: break
        if not started:
            i = next((k for k, c in enumerate(b) if c), None)
            if i is not None: started = True; b = b[i - i % fb:]
        if started: f.write(b); f.flush()
        n += 1; d = t0 + n * 0.01 - time.monotonic()
        if d > 0: time.sleep(d)
'''


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


def write_wav(path, rate, bits, seconds, data=None):
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(bits // 8)
        w.setframerate(rate)
        w.writeframes(data if data is not None else random.Random(path.name).randbytes(rate * 2 * bits // 8 * seconds))


def pcm_of(path):
    b = path.read_bytes()
    return b[b.index(b"data") + 8:]


PLAYERS = []


class Player:
    """squeezelite at `rate`/`bits` into a paced capture file."""

    def __init__(self, rate, bits, out):
        PLAYERS.append(self)
        self.out = out
        fb = 2 * bits // 8
        (work / "pacer.py").write_text(PACER)
        self.sq = subprocess.Popen(["squeezelite", "-s", f"127.0.0.1:{SLIM}", "-o", "-", "-a", str(bits),
                                    "-r", str(rate), "-n", "ci", "-m", MAC],
                                   stdout=subprocess.PIPE, stderr=open(work / "squeezelite.log", "a"))
        self.pacer = subprocess.Popen([sys.executable, "-I", str(work / "pacer.py"), str(rate), str(fb), str(out)],
                                      stdin=self.sq.stdout)
        self.sq.stdout.close()

    def stop(self):
        self.sq.terminate()
        self.sq.wait()
        self.pacer.wait(timeout=10)


def zone():
    return api("GET", f"/v1/zones/{ZONE}")


def play(track):
    api("POST", f"/v1/zones/{ZONE}/queue", {"track_ids": [track], "replace": True, "play": True})


def wait_stopped(timeout):
    until("the end of the track", lambda: zone()["transport"] == "stopped", timeout)


def bit_exact(name, track, src, rate, bits, seconds):
    p = Player(rate, bits, work / f"{name}.raw")
    until("the player", lambda: any(z["id"] == ZONE for z in api("GET", "/v1/zones")))
    time.sleep(1)
    play(track)
    np = until("playback", lambda: zone().get("now_playing"))
    wait_stopped(seconds + 30)
    time.sleep(1)
    p.stop()
    want, got = pcm_of(src), (work / f"{name}.raw").read_bytes()
    log(f"{name}: {np['source_format']['bits']}/{np['source_format']['rate_hz']} -> "
        f"{np['output_format']['bits']}/{np['output_format']['rate_hz']}; {len(want)} bytes sent, {len(got)} captured")
    if np["output_format"] != np["source_format"]:
        raise SystemExit(f"FAIL: {name}: the node converted ({np['output_format']})")
    if got[:len(want)] != want:
        n = next((i for i, (a, b) in enumerate(zip(got, want)) if a != b), min(len(got), len(want)))
        raise SystemExit(f"FAIL: {name}: differs from the source at byte {n}")
    if any(got[len(want):]):
        raise SystemExit(f"FAIL: {name}: something after the end of the track")
    log(f"{name}: identical from the first sample, then silence")


def main():
    work.mkdir(parents=True, exist_ok=True)
    music = work / "music"
    music.mkdir(exist_ok=True)
    write_wav(music / "a-noise-16-44100.wav", 44100, 16, 6)
    write_wav(music / "b-noise-24-96000.wav", 96000, 24, 5)
    tone = b"".join(struct.pack("<hh", v, v) for v in (int(3000 * math.sin(2 * math.pi * 440 * n / 44100)) for n in range(100)))
    write_wav(music / "c-tone-60s.wav", 44100, 16, 60, tone * (441 * 60))
    (work / "node.toml").write_text(f"""data_dir = "{work}/data"
[library]
folders = ["{music}"]
[api]
listen = "127.0.0.1:7813"
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
    node = subprocess.Popen([node_bin, "--config", str(work / "node.toml"), "serve"],
                            stdout=open(work / "node.log", "w"), stderr=subprocess.STDOUT)
    try:
        until("the node", lambda: api("GET", "/v1/info"))
        tracks = until("the scan", lambda: len(api("GET", "/v1/library/tracks")["tracks"]) == 3 and api("GET", "/v1/library/tracks")["tracks"])
        # By file name: titles of untagged files are tidied ("a-noise-16-44100" -> "A-noise-16-44100")
        ids = {Path(t["file"]["path"]).stem: t["id"] for t in tracks}
        bit_exact("16-44100", ids["a-noise-16-44100"], music / "a-noise-16-44100.wav", 44100, 16, 6)
        # The node keeps the capabilities a player first said it has: a new node for 96 kHz.
        node.terminate()
        node.wait()
        node = subprocess.Popen([node_bin, "--config", str(work / "node.toml"), "serve"],
                                stdout=open(work / "node.log", "a"), stderr=subprocess.STDOUT)
        until("the node again", lambda: api("GET", "/v1/info"))
        bit_exact("24-96000", ids["b-noise-24-96000"], music / "b-noise-24-96000.wav", 96000, 24, 5)

        p = Player(44100, 24, work / "tone.raw")  # the same player, back again (paced at the tone's rate)
        time.sleep(2)
        play(ids["c-tone-60s"])
        until("playback", lambda: (zone().get("now_playing") or {}).get("position_ms"))
        time.sleep(5)
        t0, p0 = time.time(), zone()["now_playing"]["position_ms"]
        time.sleep(10)
        t1, p1 = time.time(), zone()["now_playing"]["position_ms"]
        rate = (p1 - p0) / 1000 / (t1 - t0)
        log(f"position {p0} -> {p1} ms over {t1 - t0:.1f} s: {rate:.2f}x real time")
        if not 0.9 < rate < 1.1:
            raise SystemExit(f"FAIL: the position runs at {rate:.2f}x real time")

        def heard(seconds):
            """Whether the player outputs anything but silence over the next `seconds`."""
            start = (work / "tone.raw").stat().st_size
            time.sleep(seconds)
            with open(work / "tone.raw", "rb") as f:
                f.seek(start)
                return any(f.read())

        api("POST", f"/v1/zones/{ZONE}/transport", {"action": "pause"})
        time.sleep(1)
        if heard(2):
            raise SystemExit("FAIL: still playing a second after pause")
        paused_at = zone()["now_playing"]["position_ms"]
        api("POST", f"/v1/zones/{ZONE}/transport", {"action": "play"})
        time.sleep(3)
        if not heard(2):
            raise SystemExit("FAIL: silent after play")
        now = zone()["now_playing"]["position_ms"]
        log(f"pause at {paused_at} ms: silent within a second; play: playing again at {now} ms")
        if not paused_at <= now < paused_at + 8000:
            raise SystemExit(f"FAIL: resumed at {now} ms after pausing at {paused_at} ms")
        api("POST", f"/v1/zones/{ZONE}/transport", {"action": "stop"})
        p.stop()
        zones = [z for z in api("GET", "/v1/zones") if z["output"].startswith("slim:")]
        if len(zones) != 1:
            raise SystemExit(f"FAIL: {len(zones)} Slimproto zones after reconnects")
        log("PASS: bit-exact at 16/44.1 and 24/96, position in real time, pause and play, one zone through reconnects")
    finally:
        for p in PLAYERS:
            if p.sq.poll() is None:
                p.stop()
        node.terminate()


if __name__ == "__main__":
    main()
