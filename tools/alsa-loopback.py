#!/usr/bin/env python3
"""Bit-perfect check of the ALSA `hw:` output through the kernel's loopback
card (snd-aloop), as in docs/HARDWARE-TEST-PROMPT.md §C; CI runs it on every
push (ci.yml, alsa-loopback).

For each source, the node decodes it once to a WAV file (the reference) and
once plays it to `hw:Loopback,0`, while `arecord` captures `hw:Loopback,1`
in the same format. The captured PCM, after the silence before the first
sample, must hold the reference's bytes exactly.

Usage: alsa-loopback.py <pm-node> <work dir>   (needs `modprobe snd-aloop`)
"""
import random
import re
import struct
import subprocess
import sys
import time
import wave
from pathlib import Path

node, work = sys.argv[1], Path(sys.argv[2])
work.mkdir(parents=True, exist_ok=True)
FIXTURES = Path(__file__).resolve().parent.parent / "crates" / "pm-audio" / "tests" / "fixtures"


def card():
    out = subprocess.run(["aplay", "-l"], capture_output=True, text=True).stdout
    m = re.search(r"^card (\d+): Loopback", out, re.M)
    if not m:
        sys.exit("no Loopback card: sudo modprobe snd-aloop")
    return m.group(1)


def playback_output(hw):
    """The node's output id for the loopback's playback side, as `pm-node outputs` lists it."""
    out = subprocess.run([node, "outputs"], capture_output=True, text=True, timeout=60).stdout
    ids = [line.split("\t")[0] for line in out.splitlines() if line.strip()]
    want = f"alsa:hw:{hw},0"
    if want not in ids:
        sys.exit(f"the node lists no {want} (built with --features alsa?); it lists: {', '.join(ids)}")
    return want


def noise(path, rate, bits, channels, seconds=4):
    rnd = random.Random(f"{rate}/{bits}")
    width = bits // 8
    frames = rate * seconds
    data = rnd.randbytes(frames * channels * width)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(channels)
        w.setsampwidth(width)
        w.setframerate(rate)
        w.writeframes(data)
    return path


def pcm_of(wav_path):
    """The data chunk of a WAV file and its format (works for WAVE_FORMAT_EXTENSIBLE too)."""
    b = wav_path.read_bytes()
    i, fmt, data = 12, None, None
    while i + 8 <= len(b):
        cid, size = b[i:i + 4], struct.unpack("<I", b[i + 4:i + 8])[0]
        body = b[i + 8:i + 8 + size]
        if cid == b"fmt ":
            _, ch, rate, _, align, bits = struct.unpack("<HHIIHH", body[:16])
            fmt = (rate, bits, ch, align)
        elif cid == b"data":
            data = body
        i += 8 + size + (size & 1)
    return fmt, data


def first_sound(pcm, frame):
    for off in range(0, len(pcm), frame):
        if any(pcm[off:off + frame]):
            return off
    return len(pcm)


def check(src, hw, output):
    ref = work / (src.stem + ".ref.wav")
    subprocess.run([node, "play", str(src), "--to", f"wav:{ref}"], check=True, capture_output=True, timeout=120)
    (rate, bits, ch, align), want = pcm_of(ref)
    fmt = {16: "S16_LE", 24: "S24_3LE", 32: "S32_LE"}[bits]
    cap = work / (src.stem + ".cap.raw")
    rec = subprocess.Popen(["arecord", "-q", "-D", f"hw:{hw},1,0", "-f", fmt, "-r", str(rate), "-c", str(ch),
                            "-t", "raw", str(cap)])
    time.sleep(1)
    play = subprocess.run([node, "play", str(src), "--to", output], capture_output=True, text=True,
                          timeout=120)
    time.sleep(1)
    rec.terminate()
    rec.wait()
    if play.returncode != 0:
        sys.exit(f"{src.name}: play failed:\n{play.stdout}\n{play.stderr}")
    got = cap.read_bytes()
    # S24_3LE frames on both sides: 3 bytes a sample, as the WAV holds them.
    start_w, start_g = first_sound(want, align), first_sound(got, align)
    body = want[start_w:]
    ok = got[start_g:start_g + len(body)] == body
    print(f"{src.name}: {rate} Hz, {bits}-bit, {ch} ch, {fmt}: {len(body)} bytes "
          f"{'identical' if ok else 'DIFFER'} (capture silence {start_g // align} frames)")
    for line in play.stdout.splitlines():
        if re.search(r"(?i)signal|path|quality|lossy|bit", line):
            print("   ", line.strip())
    if not ok:
        n = next((i for i, (a, b) in enumerate(zip(got[start_g:], body)) if a != b), None)
        sys.exit(f"{src.name}: captured PCM differs from the reference (first difference at byte {n}, "
                 f"captured {len(got) - start_g} bytes after the silence, wanted {len(body)})")


hw = card()
output = playback_output(hw)
print(f"loopback card {hw}; playing to {output}")
sources = [
    noise(work / "noise-16-44100.wav", 44100, 16, 2),
    noise(work / "noise-24-96000.wav", 96000, 24, 2),
    FIXTURES / "14_wasted_bits.flac",
]
for s in sources:
    check(s, hw, output)
print("PASS: the ALSA hw: output is bit-perfect through the loopback")
