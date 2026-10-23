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
6. Our own network security config, allowing cleartext: the verifier's Android
   library (2.) brings one that allows http only to certificate-revocation
   hosts, and a network security config overrides usesCleartextTraffic (1.),
   so the WebView refused every http:// node (ERR_CLEARTEXT_NOT_PERMITTED).
7. Room for the system bars: Tauri's MainActivity draws edge to edge, and the
   WebView (before Chromium 136) reports no safe-area insets to the page, so
   the status bar covered the node's menu and search. The content view is
   padded by the bars' (and the keyboard's) insets, on the UI's background
   (dark or light with the system, like the web UI's theme-color).
8. A crash report readable without adb (tools/release/android/CrashReport.kt): an
   Application class installs the handler before any activity or native library
   loads; the player's notification gets a monochrome status-bar icon (an adaptive
   launcher icon there crashes some phones).

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
crash = "-keep class **.CrashReport { public static *; }\n-keep class **.PubkyMusicApp { *; }"
if player not in existing:
    existing = existing.rstrip("\n") + "\n\n# The player service's start/stop (called over JNI from src/android.rs)\n" + player + "\n"
if crash.split("\n")[0] not in existing:
    existing = existing.rstrip("\n") + "\n\n# The crash report (Application class; share() is called over JNI)\n" + crash + "\n"
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

# 6. cleartext for every host (nodes on a LAN), replacing the verifier library's config
res_xml = app / "src" / "main" / "res" / "xml"
res_xml.mkdir(parents=True, exist_ok=True)
(res_xml / "pubky_music_network.xml").write_text("""<?xml version="1.0" encoding="utf-8"?>
<!-- tools/release/android-patch.py: nodes on a LAN are http://192.168.x.y:7788. -->
<network-security-config>
    <base-config cleartextTrafficPermitted="true" />
</network-security-config>
""")
mtext = manifest.read_text()
if "pubky_music_network" not in mtext:
    tools_ns = 'xmlns:tools="http://schemas.android.com/tools"'
    if tools_ns not in mtext:
        root = '<manifest xmlns:android="http://schemas.android.com/apk/res/android"'
        if mtext.count(root) != 1:
            die(f"expected one `{root}` in {manifest}")
        mtext = mtext.replace(root, root + "\n    " + tools_ns)
    anchor = 'android:usesCleartextTraffic="${usesCleartextTraffic}"'
    if mtext.count(anchor) != 1:
        die(f"expected one `{anchor}` in {manifest}")
    mtext = mtext.replace(
        anchor,
        anchor + '\n        android:networkSecurityConfig="@xml/pubky_music_network"'
        '\n        tools:replace="android:networkSecurityConfig"',
    )
    manifest.write_text(mtext)

# 7. pad the content view by the system bars, keyboard and cutout, on the UI's background
activity = activities[0]
atext = activity.read_text()
if "pubky-music: insets" not in atext:
    call = "super.onCreate(savedInstanceState)"
    if atext.count(call) != 1 or atext.count("\n}") < 1:
        die(f"expected one `{call}` in {activity}")
    atext = atext.replace(call, call + """
    // pubky-music: insets (tools/release/android-patch.py)
    val root = findViewById<android.view.View>(android.R.id.content)
    androidx.core.view.ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
      val t = androidx.core.view.WindowInsetsCompat.Type.systemBars() or
        androidx.core.view.WindowInsetsCompat.Type.displayCutout() or
        androidx.core.view.WindowInsetsCompat.Type.ime()
      val bars = insets.getInsets(t)
      v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      androidx.core.view.WindowInsetsCompat.CONSUMED
    }
    pubkyMusicBars()""")
    # The bars follow the system theme, as the web UI does unless Settings picks one
    # (theme-color in crates/pm-api/static/index.html). uiMode changes do not recreate the activity.
    tail = atext.rstrip().rfind("}")
    atext = atext[:tail] + """
  override fun onConfigurationChanged(newConfig: android.content.res.Configuration) {
    super.onConfigurationChanged(newConfig)
    pubkyMusicBars()
  }

  private fun pubkyMusicBars() {
    val root = findViewById<android.view.View>(android.R.id.content)
    val night = (resources.configuration.uiMode and android.content.res.Configuration.UI_MODE_NIGHT_MASK) ==
      android.content.res.Configuration.UI_MODE_NIGHT_YES
    root.setBackgroundColor(if (night) 0xFF0F1014.toInt() else 0xFFF5F5F8.toInt())
    androidx.core.view.WindowCompat.getInsetsController(window, root).apply {
      isAppearanceLightStatusBars = !night
      isAppearanceLightNavigationBars = !night
    }
  }
}
"""
    activity.write_text(atext)

# 8. the crash report: its Application class, and the player notification's icon
crash_kt = (Path(__file__).parent / "android" / "CrashReport.kt").read_text()
crash_kt, n = re.subn(r"^package [\w.]+", f"package {pkg.group(1)}", crash_kt, count=1, flags=re.M)
if n != 1:
    die("CrashReport.kt has no package line")
(activities[0].parent / "CrashReport.kt").write_text(crash_kt)
mtext = manifest.read_text()
if ".PubkyMusicApp" not in mtext:
    m = re.search(r"<application\b[^>]*>", mtext, re.S)
    if not m:
        die(f"no <application> in {manifest}")
    if "android:name=" in m.group(0):
        die(f"the template's <application> already names a class: {m.group(0)[:120]}")
    mtext = mtext[: m.start()] + m.group(0).replace("<application", '<application\n        android:name=".PubkyMusicApp"', 1) + mtext[m.end():]
    manifest.write_text(mtext)
drawable = app / "src" / "main" / "res" / "drawable"
drawable.mkdir(parents=True, exist_ok=True)
(drawable / "ic_stat_pubky_music.xml").write_text("""<?xml version="1.0" encoding="utf-8"?>
<!-- tools/release/android-patch.py: the player notification's status-bar icon (a note). -->
<vector xmlns:android="http://schemas.android.com/apk/res/android"
    android:width="24dp" android:height="24dp" android:viewportWidth="24" android:viewportHeight="24">
    <path android:fillColor="#FFFFFFFF"
        android:pathData="M12,3v10.55A4,4 0,1 0,14 17V7h4V3h-6z" />
</vector>
""")

print(
    f"android-patch: multicast permission, cleartext on, rustls-platform-verifier {version}, keep rule, "
    f"player service ({pkg.group(1)}), network config, insets, crash report added"
)
