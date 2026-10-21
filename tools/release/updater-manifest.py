#!/usr/bin/env python3
"""The desktop app's update feed (latest.json, Tauri updater format) from the
signed bundles in dist/ (tools/release/collect-app.sh): one entry per
platform with the bundle's URL and the contents of its .sig.

Usage: updater-manifest.py <dist> <version> <download base URL>  > latest.json
"""
import datetime
import json
import sys
from pathlib import Path

dist, version, base = Path(sys.argv[1]), sys.argv[2], sys.argv[3].rstrip("/")
# Tauri's platform keys -> the bundle the updater installs there.
BUNDLES = {
    "darwin-aarch64": "pubky-music-macos-universal.app.tar.gz",
    "darwin-x86_64": "pubky-music-macos-universal.app.tar.gz",
    "linux-x86_64": "pubky-music-linux-x86_64.AppImage",
    "linux-aarch64": "pubky-music-linux-arm64.AppImage",
    "windows-x86_64": "pubky-music-windows-x86_64-setup.exe",
}
platforms = {}
for key, name in BUNDLES.items():
    sig = dist / f"{name}.sig"
    if not (dist / name).exists() or not sig.exists():
        print(f"updater-manifest: no {name} with its .sig; {key} gets no updates", file=sys.stderr)
        continue
    platforms[key] = {"signature": sig.read_text().strip(), "url": f"{base}/{name}"}
if not platforms:
    sys.exit("updater-manifest: no signed bundles in " + str(dist))
json.dump(
    {
        "version": version,
        "notes": "See the release notes on GitHub.",
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    },
    sys.stdout,
    indent=2,
)
print()
