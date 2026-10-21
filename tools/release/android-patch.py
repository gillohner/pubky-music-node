#!/usr/bin/env python3
"""Adjust the Android project `tauri android init` generates (gen/android is
not committed; CI generates it on every build). Each change checks its anchor
and fails loudly when Tauri's template moves, instead of building an app that
silently cannot connect.

1. Cleartext HTTP in release builds: nodes on a LAN are http://192.168.x.y:7788.
2. rustls-platform-verifier's Kotlin component (Pubky Ring sign-in needs it;
   see crates/pm-desktop/src/android.rs), at the version Cargo.lock pins.
3. ProGuard keep rules for it and the player service: R8 cannot see JNI use.
4. CHANGE_WIFI_MULTICAST_STATE: the Wi-Fi multicast lock that LAN discovery
   (mDNS) needs while it looks for nodes (src/android.rs).
5. The player's foreground service (tools/release/android/PlayerService.kt),
   its permissions and manifest entry: it keeps the phone's zone playing with
   the app in the background (src/android.rs starts it).

Usage: android-patch.py <crates/pm-desktop>
"""
import re
import sys
from pathlib import Path

root = Path(sys.argv[1] if len(sys.argv) > 1 else "crates/pm-desktop")
app = root / "gen" / "android" / "app"
gradle = app / "build.gradle.kts"
proguard = app / "proguard-rules.pro"
MARK = "// pubky-music: patched"


def die(msg: str) -> None:
    print(f"android-patch: {msg}", file=sys.stderr)
    sys.exit(1)


if not gradle.exists():
    die(f"{gradle} missing: run `tauri android init` first")
text = gradle.read_text()
if MARK in text:
    print("android-patch: already patched")
    sys.exit(0)

# 1. cleartext
old = 'manifestPlaceholders["usesCleartextTraffic"] = "false"'
if text.count(old) != 1:
    die(f"expected one `{old}` in {gradle}")
text = text.replace(old, 'manifestPlaceholders["usesCleartextTraffic"] = "true"')

# 2. the verifier's Kotlin component, version from Cargo.lock
lock = (root / "Cargo.lock").read_text()
m = re.search(r'name = "rustls-platform-verifier-android"\nversion = "([^"]+)"', lock)
if not m:
    die("rustls-platform-verifier-android not in Cargo.lock")
version = m.group(1)
text += f"""
{MARK}
repositories {{
    maven {{
        url = uri("https://github.com/rustls/rustls-platform-verifier/raw/maven-archive/android-release-support/maven/")
    }}
}}
dependencies {{
    implementation("org.rustls:rustls-platform-verifier:{version}")
}}
"""
gradle.write_text(text)

# 3. keep rule
rule = "-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }"
existing = proguard.read_text() if proguard.exists() else ""
if rule not in existing:
    existing = existing.rstrip("\n") + "\n\n# rustls-platform-verifier (called over JNI)\n" + rule + "\n"
player = "-keep class **.PlayerService { public static void start(...); public static void stop(...); }"
if player not in existing:
    existing = existing.rstrip("\n") + "\n\n# The player service's start/stop (called over JNI from src/android.rs)\n" + player + "\n"
proguard.write_text(existing)

# 4. multicast permission
manifest = app / "src" / "main" / "AndroidManifest.xml"
mtext = manifest.read_text()
perm = '<uses-permission android:name="android.permission.CHANGE_WIFI_MULTICAST_STATE" />'
if perm not in mtext:
    anchor = '<uses-permission android:name="android.permission.INTERNET" />'
    if mtext.count(anchor) != 1:
        die(f"expected one `{anchor}` in {manifest}")
    manifest.write_text(mtext.replace(anchor, anchor + "\n    " + perm))

# 5. the player's foreground service, in MainActivity's package
activities = list((app / "src" / "main" / "java").rglob("MainActivity.kt"))
if len(activities) != 1:
    die(f"expected one MainActivity.kt, found {len(activities)}")
pkg = re.search(r"^package ([\w.]+)", activities[0].read_text(), re.M)
if not pkg:
    die(f"no package line in {activities[0]}")
service = (Path(__file__).parent / "android" / "PlayerService.kt").read_text()
service, n = re.subn(r"^package [\w.]+", f"package {pkg.group(1)}", service, count=1, flags=re.M)
if n != 1:
    die("PlayerService.kt has no package line")
(activities[0].parent / "PlayerService.kt").write_text(service)
mtext = manifest.read_text()
anchor = '<uses-permission android:name="android.permission.INTERNET" />'
perms = [
    "android.permission.FOREGROUND_SERVICE",
    "android.permission.FOREGROUND_SERVICE_MEDIA_PLAYBACK",
    "android.permission.POST_NOTIFICATIONS",
    "android.permission.WAKE_LOCK",
    "android.permission.ACCESS_WIFI_STATE",
]
for p in perms:
    line = f'<uses-permission android:name="{p}" />'
    if line not in mtext:
        mtext = mtext.replace(anchor, anchor + "\n    " + line)
entry = (
    '<service android:name=".PlayerService" android:exported="false"'
    ' android:foregroundServiceType="mediaPlayback" />'
)
if entry not in mtext:
    if mtext.count("</application>") != 1:
        die(f"expected one </application> in {manifest}")
    mtext = mtext.replace("</application>", "    " + entry + "\n    </application>")
manifest.write_text(mtext)

print(
    f"android-patch: multicast permission, cleartext on, rustls-platform-verifier {version}, keep rule, "
    f"player service ({pkg.group(1)}) added"
)
