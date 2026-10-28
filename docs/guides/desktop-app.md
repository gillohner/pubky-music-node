# The desktop and Android app

The Pubky Music app is the node's web page in its own window, for macOS, Windows, Linux and
Android. It connects to a node you have installed (see the [README](../../README.md#choose-how-to-run-it));
it doesn't hold your library itself. What it adds over a browser:

- it finds your node on the home network, and from anywhere once the node is signed in to Pubky;
- on a computer, it plays through that computer's sound outputs and DACs;
- on Android, it plays on the phone (speaker, headphones, Bluetooth, a USB DAC) and can keep
  music for offline listening.

## Install

Download from the [latest release](https://github.com/gillohner/pubky-music-node/releases/latest):

| System | File | First start |
| :-- | :-- | :-- |
| macOS | `pubky-music-macos-universal.dmg` | Drag it to Applications. The app isn't signed by Apple: the first time, right-click it and choose **Open**. If macOS still refuses, open **System Settings › Privacy & Security** and click **Open Anyway**. |
| Windows | `pubky-music-windows-x86_64-setup.exe` | The installer isn't signed: choose **More info**, then **Run anyway**. |
| Linux | `.deb` (Debian, Ubuntu) or `.AppImage` (others), x86_64 or arm64 | `sudo apt install ./pubky-music-linux-x86_64.deb`, or make the AppImage executable and run it. |
| Android, Google TV | `pubky-music-android-arm64.apk` | Open the file on the device and allow installs from that source. |

## Connect to your node

The app opens on a connect page. Use one of these:

- **On this network:** nodes on your home network are listed. Click yours and enter the access
  token once.
- **Or connect by address:** type the node's address (for example `192.168.1.10:7788`) and the token.
- **Sign in with Pubky Ring:** once the node is signed in to Pubky (**Settings › My accounts**
  on the node), the app finds it through your Pubky account, at home or away.

The app remembers the node. If the node's address changes (a new IP from the router), the app
finds it again on the network by itself.

## Playing on this computer or phone

While connected at home, the app adds this device as a speaker on the node: "<computer name> ·
<sound output>" on a computer, "<phone model> · Speaker or headphones" on Android. Pick it with the
zone button in the player bar, like any other speaker. On Linux it plays bit-perfect to a DAC; on
macOS and Windows it goes through the system's sound output.

Away from home the app still controls the node and its speakers, but this device isn't offered as
a speaker then.

## Updates

On macOS, Windows and the Linux AppImage, the app checks for a new version when it starts and
asks before installing it. The Linux `.deb` is updated like other packages: install the new
`.deb` over the old one. On Android, install the new `.apk` from the release page over the old
one.

## Subsonic apps instead

Any OpenSubsonic app (Symfonium and Amperfy on phones, Feishin on desktops) works with the node
too: server `http://<node address>:7788`, user `pubky`, and the access token as the password.
