# Backups and updates

## What to back up

Your music files are yours to back up as usual; the node never changes them. The node's own data
is the library database (matches, playlists, favourites, tags, play history, queues, settings) and
your sign-ins to services. If you signed in to Pubky, playlists, favourites and tags are also kept
in your Pubky account and come back on a new node by themselves.

With the node's own homeserver (the `deploy/bundle` install), back up the homeserver too: its
database, its key and your Pubky account's files. `backup.sh` below does all of it at once.

## A backup file from Settings

**Settings › Backup › Download a backup** makes one file, `pubky-music-<date>-<time>.pmbackup`.

- Without a passphrase it holds everything except your sign-ins (TIDAL, Spotify, ListenBrainz,
  Pubky and so on). After a restore the node lists which services to sign in to again.
- With a passphrase the sign-ins are included, encrypted. Nothing opens them without the
  passphrase, so keep it somewhere safe.

The file holds the node's library only, never a homeserver.

**Every day or week, to a folder:** in the same place, pick a folder on the node, how often and
how many to keep. Best on another disk or a NAS. In Docker the folder must be inside the
container; add a volume for it, for example `- /mnt/backup/pubky-music:/backups` under `volumes`
(writable by user id 1000), and enter `/backups`. On Linux without Docker, the service may only
write to its own folder: add the backup folder to the `ReadWritePaths=` line of
`/etc/systemd/system/pubky-music-node.service` (separated by a space), make it writable by the
`pubky-music` user, then `sudo systemctl daemon-reload && sudo systemctl restart pubky-music-node`.

## The node with its own homeserver: `backup.sh`

In the bundle folder, while the bundle runs:

```sh
cd ~/pubky-music-node/deploy/bundle     # the folder with docker-compose.yml and .env
./backup.sh                             # into ~/backups/pubky-music/<date-time>
```

It needs nothing but Docker and takes a few seconds; the music is never copied. Each run is a new
folder (about 100 MB for a library of 16 000 songs, most of it covers). The newest 7 are kept:
`KEEP=14 ./backup.sh` keeps 14, and `./backup.sh /mnt/nas/pubky-music` writes somewhere else. It
stops before writing when the disk is short of space.

| In the folder | What it is |
| :-- | :-- |
| `config/` | `.env`, `docker-compose.yml`, `docker-compose.override.yml`, the images that ran, where the music is mounted from (`music-mounts.tsv`) and `recreate-volumes.sh` |
| `node/library.pmbackup` | the backup file from Settings, with your sign-ins locked under a passphrase |
| `node/passphrase` | that passphrase, made for this backup (unless `PM_BACKUP_PASSPHRASE` is set in `.env`) |
| `node/library.db` | the whole library database (it keeps the node's id; without the sign-ins) |
| `node/data.tar.gz`, `node/tunnel.tar.gz` | the rest of the node's data folder (Pubky session, remote-access key, vault key, config, covers) and the tunnel settings |
| `homeserver/database.dump` | the homeserver's PostgreSQL database (accounts, sessions, file index) |
| `homeserver/data.tar.gz` | the homeserver's key, config and your account's files |

The folder holds every secret of the node (`.env`, the sign-ins, the homeserver's key), so it is
readable by you only. Anyone with the folder can run your node and your homeserver. On a NAS share
the share's own permissions decide who can read it (the script warns when others can): use a share
only you can open.

**Every night:** run `mkdir -p ~/backups`, then `crontab -e` as the user that runs Docker, and add

```
30 3 * * * $HOME/pubky-music-node/deploy/bundle/backup.sh >> $HOME/backups/pubky-music.log 2>&1
```

### Music on network shares

The node never backs up the music on a NAS or another server; back it up there. What it needs to
find the music again is in the backup:

- shares mounted by Docker (a `cifs` volume declared `external: true` in
  `docker-compose.override.yml`): the override, and `config/recreate-volumes.sh`, which makes each
  volume again with the address, share and options it had (a share password, if you used one, is in
  there too, as it is in Docker's own volume settings);
- shares the machine mounts itself (`/etc/fstab`, a NAS app): `config/host-shares.txt` and the
  matching `/etc/fstab` lines in `config/fstab-lines.txt`. Mount them again before the restore.

The library remembers each song by its path in the container (`/music/Roon/...`). Keep the same
mount targets and every match, playlist and play stays with its song; for a new place, see
[Restore a backup file](#restore-a-backup-file).

### Restore on a new disk or machine

1. Install Docker, then get the bundle: `git clone https://github.com/gillohner/pubky-music-node`
   and `cd pubky-music-node/deploy/bundle`. Don't run `setup.sh`: the passwords come from the
   backup.
2. Put the music back where it was (or mount the shares from `host-shares.txt`), then
   `./restore.sh ~/backups/pubky-music/<date-time>`.

`restore.sh` checks the files against their checksums, puts back `.env` and the override, makes
the share volumes and their mount points, restores the homeserver's database and files and the
node's data, starts everything, and after the node's first scan puts the sign-ins back through the
node's own restore. It ends with the node's report: the sign-ins it put back and any library folder
it cannot find. It only restores into an empty install: it stops when the bundle already has
containers or data, so it never overwrites a running node.

On a machine with another network address, change `PUBLIC_IP` and `PM_PUBLIC_URL` in `.env` after
the restore, then `docker compose up -d`. The dashboard and the Cloudflare tunnel start again with
`docker compose --profile dashboard --profile tunnel up -d`, as after the first install.

If `restore.sh` stops half-way, it says how to start over: `docker compose down -v` removes the
half-restored install (only this one; the backup folder is never changed), then run it again.

## Restore a backup file

On a new or reinstalled node: Home's **Get started** card links to it, or **Settings › Backup ›
Choose a backup file**. The node shows what the file holds first, then puts the library back and
rescans. If your music now lives at another path, it asks for each folder's new place, and the
tracks keep their playlists and history.

## Copying the whole data folder

The node's data folder holds everything, including the access token and the key for remote
access. Stop the node before copying it. Copying it on a NAS needs a terminal (SSH);
the backup file above doesn't.

| Install | Data folder |
| :-- | :-- |
| Docker ([install guide](install-docker.md)) | the `pubky-music-data` volume (`/data` in the container) |
| Docker with its own homeserver (`deploy/bundle`) | the `pubky-music_music-node` volume; the homeserver is in `pubky-music_homeserver` and `pubky-music_postgres` (use `backup.sh`) |
| Linux without Docker | `/var/lib/pubky-music` |

## Updating

Make a backup first. Your library and settings are kept across updates.

**Docker:**

```sh
cd ~/pubky-music          # the folder with docker-compose.yml
docker compose pull
docker compose up -d
```

**Docker with its own homeserver:** the node's version is `PM_TAG` in `.env`: `edge` (the newest
code), `latest` (the last release) or a release number such as `0.2.0`. To change it, edit `.env`
first.

```sh
cd ~/pubky-music-node/deploy/bundle
./backup.sh
git pull                  # newer homeserver and database images come with the bundle
docker compose pull
docker compose up -d
```

`git pull` keeps `.env` and `docker-compose.override.yml`.

With a NAS app (Synology, Portainer, TrueNAS), use its "pull" or "update" button for the
project, or redeploy it.

**Linux without Docker:** download the new release file and install `pm-node` again as in
[step 1](install-linux.md#1-download-and-install), then `sudo systemctl restart pubky-music-node`.

**The desktop app** asks when a new version is out (macOS, Windows, AppImage). Install a new
`.deb` or Android `.apk` over the old one.

The version a node runs is in **Settings › About and updates**. What changed:
[docs/CHANGELOG.md](../CHANGELOG.md).

### Going back to the previous version

Set the image tag back (`PM_TAG` in the bundle's `.env`, or the `image:` line in your own compose
file) and run `docker compose pull && docker compose up -d` again.

When the newer version changed the library's database (its CHANGELOG entry says so), put back the
library from the backup you made before updating; versions after 0.2.0 refuse to start on a
library a newer version changed ("this library was opened by a newer version of Pubky Music").
With the bundle:

```sh
cd ~/pubky-music-node/deploy/bundle
docker compose stop music
docker run --rm -i -v pubky-music_music-node:/v alpine:3.22 sh -c \
  'cp /v/library.db /v/library.db.newer && cat > /v/library.db && rm -f /v/library.db-wal /v/library.db-shm && chown "$(stat -c %u:%g /v)" /v/library.db' \
  < ~/backups/pubky-music/<date-time>/node/library.db
docker compose start music
```

Then restore that backup's `node/library.pmbackup` in **Settings › Backup › Choose a backup file**
with the passphrase from `node/passphrase`: it puts the sign-ins back. Anything you changed after
that backup is gone (the newer library stays next to it as `library.db.newer`). A homeserver that won't start after `git pull`: go back to the bundle you had
with `git checkout <the commit in the backup's config/git-commit>` and `docker compose up -d`. If
it still fails, [ask for help](troubleshooting.md#still-stuck) before removing anything; the
backup folder stays as it is.
