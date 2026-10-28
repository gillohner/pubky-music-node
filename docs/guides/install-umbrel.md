# Install on Umbrel

The Umbrel app installs the node together with its own Pubky homeserver, so your Pubky account,
playlists and uploaded music can live on the Umbrel box.

## 1. Add the app store

1. In umbrelOS, open the **App Store**.
2. Click **⋯** (top right) › **Community App Stores**.
3. Paste `https://github.com/gillohner/pubky-music-umbrel` and add it.
4. Open the **Pubky Music** store and install **Pubky Music Node**.

Install this app or Pubky's own "Pubky Homeserver" app, not both: both use port 6287.

## 2. Open it

1. Open the app from the Umbrel home screen.
2. The page asks for the API token. It is the password umbrelOS shows for this app (right-click
   the app's icon on the home screen). Paste it and press **Connect**.
3. On Home, the **Get started** card offers **Create account here** (it is also in **Settings**,
   with your accounts). The Pubky Ring app on your phone creates your Pubky account on this
   Umbrel and signs the node in.

The same address (`http://umbrel.local:7788` or the box's IP with `:7788`) and token work in the
Pubky Music app and in Subsonic apps (user `pubky`).

## Settings names in version 0.1.0

The guides use the Settings names of the current version. The Umbrel app may still run 0.1.0,
where some tabs have other names:

| In the guides | In 0.1.0 |
| :-- | :-- |
| Library folders | Library |
| Zones and outputs | Playback |
| My accounts | Accounts |
| Domain and tunnel | Remote access |
| Display | Appearance |
| About and updates | About this node |

**Settings › About** shows which version runs.

## Where the music comes from

- **Umbrel's shared storage** (the Files app, downloads from other Umbrel apps) is visible to the
  node, read-only, as `/music`. In **Settings › Library folders** (called **Library** in version
  0.1.0), type `/music` and press **Add folder**. New files are picked up about 15 seconds after
  they land.
- **Add music**, a button on the Albums and Tracks pages, uploads files from your browser, or
  copies a folder, into your homeserver's private storage. The node plays them from there without
  a second copy.

## Speakers

The app runs on the host network, so speakers on your home network show up by themselves. A USB
DAC plugged into the Umbrel box shows up as a speaker too. See [Speakers](speakers.md).

## Updates

When a new version is out, the App Store shows an update for Pubky Music Node. Your library and
settings are kept. Make a backup first: [Backups and updates](backup-and-update.md).

## Away from home

The Pubky Music app reaches the node from anywhere once you are signed in. For a browser away from
home, set up a Cloudflare Tunnel from Settings:
[Remote access](remote-access.md#cloudflare-tunnel-umbrel-and-the-bundle).
