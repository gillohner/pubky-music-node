#!/usr/bin/env python3
"""The phone as a zone, on an Android emulator (CI: release.yml, android-emulator).

The release APK is installed and connected by address to a node on the host
(10.0.2.2 from the emulator) over plain http, and must show the node's page.
Its built-in player (AAudio through cpal)
must become a zone that plays a track in real time, and keep playing with the
app in the background: the foreground service (PlayerService.kt) keeps the
process alive.

The emulator is behind its own NAT, so the node cannot reach the address the
app registers itself under. This script forwards a host port to the phone's
player with `adb forward` and registers that instead; the registration call
itself is the desktop app's, tested there.

Then, as a phone does on later days:
- on Android 13+ the app is installed without granting anything, and the notification
  permission is refused, as a cautious owner would;
- the app is stopped and started again: it reconnects to the saved node by itself and its
  player service starts again (the path a first-launch test never takes);
- the app is made to crash (`am crash`): on the next launch it shows the crash report
  ("closed unexpectedly", tools/release/android/CrashReport.kt), and Continue carries on.

Usage: android-endpoint.py <apk> <pm-node binary> <out dir>
"""
import json
import os
import re
import subprocess
import sys
import time
import urllib.request
import wave
from pathlib import Path

PKG = "to.synonym.pubkymusic"
NODE = os.environ.get("PM_TEST_NODE", "http://127.0.0.1:7788")
apk, node_bin, out = sys.argv[1], sys.argv[2], Path(sys.argv[3])


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def adb(*args, check=True):
    r = subprocess.run(["adb", *args], capture_output=True, text=True, timeout=120)
    if check and r.returncode != 0:
        raise RuntimeError(f"adb {' '.join(args)}: {r.stderr.strip() or r.stdout.strip()}")
    return r.stdout


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(NODE + path, data=data, method=method, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{method} {path}: {e.code} {e.read().decode(errors='replace')}") from None
    return json.loads(raw) if raw else None


def until(what, fn, timeout=60, every=1.0):
    end = time.time() + timeout
    while True:
        try:
            v = fn()
            if v:
                return v
        except Exception as e:  # noqa: BLE001 (retried until the deadline)
            last = e
        else:
            last = None
        if time.time() > end:
            raise RuntimeError(f"timed out waiting for {what}" + (f" (last error: {last})" if last else ""))
        time.sleep(every)


def logcat():
    return (out / "logcat.txt").read_text(errors="replace") if (out / "logcat.txt").exists() else ""


PROMPT = re.compile(r'text="(Don.t allow|Deny)"')


def ui_nodes():
    """The screen's nodes; Android 13+'s notification prompt, whenever it shows, is refused."""
    for _ in range(3):
        adb("shell", "uiautomator", "dump", "/sdcard/ui.xml")
        xml = adb("exec-out", "cat", "/sdcard/ui.xml")
        nodes = re.findall(r"<node [^>]*>", xml)
        prompt = next((n for n in nodes if PROMPT.search(n)), None)
        if not prompt:
            return nodes
        x, y = center(prompt)
        adb("shell", "input", "tap", str(x), str(y))
        log("notifications refused")
        time.sleep(1)
    return nodes


def center(node):
    m = re.search(r'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', node)
    x1, y1, x2, y2 = map(int, m.groups())
    return (x1 + x2) // 2, (y1 + y2) // 2


def tap(pred, what):
    def find():
        for n in ui_nodes():
            if pred(n):
                return n
        return None

    node = until(what, find, timeout=90, every=2)
    x, y = center(node)
    adb("shell", "input", "tap", str(x), str(y))
    log(f"tapped {what} at {x},{y}")


def shot(name):
    png = subprocess.run(["adb", "exec-out", "screencap", "-p"], capture_output=True, timeout=60).stdout
    (out / f"{name}.png").write_bytes(png)


def check_status_bar():
    """The page starts below the status bar (Tauri draws edge to edge; android-patch.py pads)."""
    win = adb("shell", "dumpsys", "window")
    bars = [int(m.group(1)) for m in re.finditer(r"type=statusBars[^\n]*?frame=\[\d+,\d+\]\[\d+,(\d+)\]", win)]
    menu = next((n for n in ui_nodes() if 'content-desc="Menu"' in n or 'text="Menu"' in n), None)
    if not bars or not menu:
        log(f"status bar check skipped (status bar frame found: {bool(bars)}, menu found: {bool(menu)})")
        return
    top = int(re.search(r'bounds="\[\d+,(\d+)\]', menu).group(1))
    assert top >= max(bars), f"the status bar (to y={max(bars)}) covers the node's menu (top y={top})"
    log(f"the node's menu (y={top}) is below the status bar (y<{max(bars)})")


def zone_state(zid):
    return api("GET", f"/v1/zones/{zid}")


def position(zid):
    z = zone_state(zid)
    if z.get("error"):
        raise RuntimeError(f"zone error: {z['error']}")
    np = z.get("now_playing") or {}
    return z.get("transport"), np.get("position_ms")


def rate_over(zid, seconds):
    t0, (_, p0) = time.time(), position(zid)
    time.sleep(seconds)
    t1, (tr, p1) = time.time(), position(zid)
    if p0 is None or p1 is None:
        raise RuntimeError(f"no position (transport {tr})")
    rate = (p1 - p0) / 1000 / (t1 - t0)
    log(f"position {p0} -> {p1} ms over {t1 - t0:.1f} s: {rate:.2f}x real time ({tr})")
    return rate, tr


def main():
    (out / "music").mkdir(parents=True, exist_ok=True)
    (out / "data").mkdir(exist_ok=True)
    # Two minutes of a quiet 440 Hz tone, 16-bit 44.1 kHz stereo.
    import math
    import struct

    with wave.open(str(out / "music" / "tone.wav"), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(44100)
        frame = [struct.pack("<hh", v, v) for v in (int(2000 * math.sin(2 * math.pi * 440 * n / 44100)) for n in range(100))]
        w.writeframes(b"".join(frame) * (441 * 120))
    (out / "node.toml").write_text(f"""data_dir = "{out}/data"
[library]
folders = ["{out}/music"]
[api]
listen = "{os.environ.get('PM_TEST_LISTEN', '0.0.0.0:7788')}"
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
    node = subprocess.Popen([node_bin, "--config", str(out / "node.toml"), "serve"],
                            stdout=open(out / "node.log", "w"), stderr=subprocess.STDOUT)
    try:
        run(node)
    finally:
        node.terminate()


def run(node):
    until("the node", lambda: api("GET", "/v1/info"))
    api("POST", "/v1/library/scan", {})

    def tracks():
        t = api("GET", "/v1/library/tracks")
        items = t if isinstance(t, list) else t.get("items") or t.get("tracks") or []
        return items

    track = until("the scan", tracks)[0]
    log(f"node up, track {track['id']}")

    sdk = int(adb("shell", "getprop", "ro.build.version.sdk").strip() or 0)
    log(f"Android API {sdk}")
    # Android 13+: nothing granted up front; the notification prompt is refused below
    log(adb("install", "-r", *([] if sdk >= 33 else ["-g"]), apk).strip())
    adb("logcat", "-c")
    lc = subprocess.Popen(["adb", "logcat", "-v", "time", "pubky-music:V", "AndroidRuntime:E", "ActivityManager:I", "*:S"],
                          stdout=open(out / "logcat.txt", "w"), stderr=subprocess.STDOUT)
    try:
        adb("shell", "am", "start", "-W", "-n", f"{PKG}/.MainActivity")
        tap(lambda n: "connect by address" in n, "“Or connect by address”")
        # The address field is the page's first text field (its id and hint differ between WebViews)
        tap(lambda n: 'resource-id="url"' in n or "EditText" in n, "the address field")
        adb("shell", "input", "text", "10.0.2.2:7788")
        adb("shell", "input", "keyevent", "66")
        shot("1-connecting")
        def node_page():
            nodes = "\n".join(ui_nodes())
            if "Webpage not available" in nodes or "ERR_" in nodes:
                err = re.search(r'text="(net::ERR_[A-Z_]+)"', nodes)
                raise SystemExit(f"FAIL: the app cannot load the node's page ({err.group(1) if err else 'Webpage not available'})")
            return "Now playing" in nodes

        until("the node's page in the app", node_page, timeout=90, every=3)
        log("the node's page is showing in the app")
        check_status_bar()
        shot("2a-light")
        adb("shell", "cmd", "uimode", "night", "yes")  # the bars and the page follow to dark
        time.sleep(3)
        shot("2b-dark")
        adb("shell", "cmd", "uimode", "night", "no")
        until("the player service", lambda: "player service started" in logcat(), timeout=90)
        m = until("the player's port", lambda: re.search(r"player ready port=(\d+)", logcat()), timeout=10)
        port = m.group(1)
        log(f"app connected; player on port {port}; service started")
        shot("2-connected")

        adb("forward", "tcp:17790", f"tcp:{port}")
        outs = api("POST", "/v1/outputs/endpoints", {"address": "127.0.0.1:17790"})
        log("phone outputs: " + ", ".join(o["name"] for o in outs))
        want = os.environ.get("PM_TEST_OUTPUT", "Speaker or headphones")
        phone = next((o for o in outs if want.lower() in o["name"].lower()), None)
        assert phone, f"no output named like {want!r}"
        zone = until("the phone's zone", lambda: next((z for z in api("GET", "/v1/zones") if z["output"] == phone["id"]), None))
        zid = zone["id"]
        api("POST", f"/v1/zones/{zid}/queue", {"track_ids": [track["id"]], "replace": True, "play": True})
        until("playback", lambda: position(zid)[1], timeout=30)
        z = zone_state(zid)
        np = z["now_playing"]
        log(f"playing on {z['name']}: {np['source_format']} -> {np['output_format']}")

        rate, tr = rate_over(zid, 10)
        assert tr == "playing" and 0.8 < rate < 1.2, f"foreground: {rate:.2f}x ({tr})"

        adb("shell", "input", "keyevent", "3")  # HOME: the app goes to the background
        log("app in the background")
        time.sleep(15)
        rate, tr = rate_over(zid, 15)
        services = adb("shell", "dumpsys", "activity", "services", PKG)
        (out / "services.txt").write_text(services)
        assert "PlayerService" in services and "isForeground=true" in services, "the player service is not in the foreground"
        assert tr == "playing" and 0.8 < rate < 1.2, f"background: {rate:.2f}x ({tr})"
        shot("3-background")
        api("POST", f"/v1/zones/{zid}/transport", {"action": "stop"})
        log("the phone is a zone, plays in real time, and keeps playing in the background")

        # Started again with the node saved: it reconnects and the service starts again
        adb("shell", "am", "force-stop", PKG)
        (out / "logcat.txt").write_text("")
        adb("logcat", "-c")
        adb("shell", "am", "start", "-W", "-n", f"{PKG}/.MainActivity")
        until("the node's page after a restart", node_page, timeout=90, every=3)
        until("the player service after a restart", lambda: "player service started" in logcat(), timeout=90)
        shot("4-restarted")
        log("restarted: reconnected to the saved node, player service started")

        # A crash: the next launch shows the report, and Continue carries on
        crashed = subprocess.run(["adb", "shell", "am", "crash", PKG], capture_output=True, text=True, timeout=60)
        if crashed.returncode != 0 or "Unknown command" in crashed.stdout + crashed.stderr:
            log(f"crash report check skipped: `am crash` is not available here ({crashed.stderr.strip()})")
        else:
            time.sleep(3)
            adb("shell", "am", "start", "-W", "-n", f"{PKG}/.MainActivity")
            until("the crash report", lambda: any("closed unexpectedly" in n for n in ui_nodes()), timeout=60, every=2)
            shot("5-crash-report")
            # A release build cannot be read with run-as: read the report off the screen
            report = "\n".join(re.findall(r'text="([^"]*)"', "\n".join(ui_nodes())))
            (out / "crash-report.txt").write_text(report)
            assert "Java exception" in report or "crashed" in report, f"the report says: {report[:400]!r}"
            log("the crash report shows on the next launch")
            tap(lambda n: 'text="Continue"' in n, "Continue")
            until("the node's page after the report", node_page, timeout=90, every=3)
            log("Continue reconnects to the node")
        log("PASS: the phone is a zone, plays in real time and in the background, survives a restart, and reports a crash")
    finally:
        lc.terminate()


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        log(f"FAIL: {e!r}")
        try:
            shot("fail")
        except Exception:  # noqa: BLE001
            pass
        sys.exit(1)
