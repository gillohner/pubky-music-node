# Backups and updates

## What to back up

Your music files are yours to back up as usual; the node never changes them. The node's own data
is the library database (matches, playlists, favourites, tags, play history, queues, settings) and
your sign-ins to services. If you signed in to Pubky, playlists, favourites and tags are also kept
in your Pubky account and come back on a new node by themselves.

## A backup file from Settings

**Settings › Backup › Download a backup** makes one file, `pubky-music-<date>-<time>.pmbackup`.

- Without a passphrase it holds everything except your sign-ins (TIDAL, Spotify, ListenBrainz,
  Pubky and so on). After a restore the node lists which services to sign in to again.
- With a passphrase the sign-ins are included, encrypted. Nothing opens them without the
  passphrase, so keep it somewhere safe.

**Every day or week, to a folder:** in the same place, pick a folder on the node, how often and
how many to keep. Best on another disk or a NAS. In Docker the folder must be inside the
container; add a volume for it, for example `- /mnt/backup/pubky-music:/backups` under `volumes`
(writable by user id 1000), and enter `/backups`. On Linux without Docker, the service may only
write to its own folder: add the backup folder to the `ReadWritePaths=` line of
`/etc/systemd/system/pubky-music-node.service` (separated by a space), make it writable by the
`pubky-music` user, then `sudo systemctl daemon-reload && sudo systemctl restart pubky-music-node`.

## Restore

On a new or reinstalled node: Home's **Get started** card links to it, or **Settings › Backup ›
Choose a backup file**. The node shows what the file holds first, then puts the library back and
rescans. If your music now lives at another path, it asks for each folder's new place, and the
tracks keep their playlists and history.

## Copying the whole data folder

The node's data folder holds everything, including the access token and the key for remote
access. Stop the node before copying it. Copying it on a NAS or Umbrel needs a terminal (SSH);
the backup file above doesn't.

| Install | Data folder |
| :-- | :-- |
| Docker | the `pubky-music-data` volume (`/data` in the container) |
| Umbrel | `umbrel/app-data/pubky-music-node/data/node` (the homeserver and its database are next to it) |
| Linux without Docker | `/var/lib/pubky-music` |

## Updating

Make a backup first. Your library and settings are kept across updates.

**Docker:**

```sh
cd ~/pubky-music          # the folder with docker-compose.yml
docker compose pull
docker compose up -d
```

With a NAS app (Synology, Portainer, TrueNAS), use its "pull" or "update" button for the
project, or redeploy it.

**Umbrel:** the App Store shows an update for Pubky Music Node when there is one.

**Linux without Docker:** download the new release file and install `pm-node` again as in
[step 1](install-linux.md#1-download-and-install), then `sudo systemctl restart pubky-music-node`.

**The desktop app** asks when a new version is out (macOS, Windows, AppImage). Install a new
`.deb` or Android `.apk` over the old one.

The version a node runs is in **Settings › About and updates**. What changed:
[docs/CHANGELOG.md](../CHANGELOG.md).
