# App test prompt: the Pubky Music app on macOS, Windows, Linux and Android

The app (`crates/pm-desktop`, Tauri 2, ADR 0012) has run only on Linux under Xvfb against a test
node. Paste this into Claude Code on a machine with a display (or follow it by hand), with a node
running on the LAN.

```
Download the app from https://github.com/gillohner/pubky-music-node/releases/tag/edge for this
machine (macOS .dmg, Windows setup .exe, Linux .AppImage or .deb, Android arm64 .apk). Report
pass/fail with a screenshot or log line for each check, and the exact OS version and hardware.

1. Install and start. macOS: right-click → Open the first time (unsigned). Windows: SmartScreen →
   More info → Run anyway. Android: allow installs from the browser or file manager.
2. Connect by address: "Or connect by address", the node's http://<ip>:7788 and its token. A wrong
   token says "The node refused the token"; the right one opens the node's UI, signed in (no second
   token prompt).
3. Quit and start again: the UI opens by itself. Stop the node, start the app: "Can't reach …" with
   "Try again" and "Connect to another node"; start the node, Try again reconnects.
4. Sign in with Pubky Ring (after "Connect to another node"): desktop shows a QR to scan with Ring
   on the phone; Android's "Open Pubky Ring" hands over to Ring on the same phone. After approval
   the node's UI opens. This needs a node signed in once on its own web page (Settings → Pubky).
5. Links: an "Open on …" link (Discover, an album's Versions) opens in the system browser; the app
   stays on the node.
6. Desktops only, the built-in player: Zones lists "<this computer> · <DAC>". Play an album to it;
   on Linux with the DAC free of PipeWire the signal path is bit-perfect. The node must reach this
   machine on TCP 7790 (firewall prompts on macOS and Windows: allow).
7. Android only: phone and Google TV (if any) both start and connect; the UI is usable at phone
   width and with a TV remote (D-pad focus moves through the UI).
```
