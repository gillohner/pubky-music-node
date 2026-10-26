# Pubky Music for Umbrel

A community app store for [umbrelOS](https://umbrel.com) with one app: **Pubky Music Node**, the music
server with its own Pubky homeserver (PostgreSQL, pubky-homeserver, an optional Cloudflare tunnel).

**Add it to Umbrel:** App Store → ⋯ (top right) → Community App Stores → add
`https://github.com/gillohner/pubky-music-umbrel`, then open the "Pubky Music" store and install the
app. The password Umbrel shows for the app is the node's token: the web UI, the apps and Subsonic
clients ask for it.

This repository is a mirror of [`deploy/umbrel/`](https://github.com/gillohner/pubky-music-node/tree/main/deploy/umbrel)
in [gillohner/pubky-music-node](https://github.com/gillohner/pubky-music-node), synced on every change
there. Report problems and send changes there, not here. How it's set up and checked:
[docs/deploy.md › Umbrel](https://github.com/gillohner/pubky-music-node/blob/main/docs/deploy.md#umbrel).
