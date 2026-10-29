#!/usr/bin/env bash
# Backs up this bundle while it runs (docs/guides/backup-and-update.md): the node's library,
# settings and sign-ins, the homeserver (its database, its key, the users' files), the tunnel
# settings, .env, the compose files and the definitions of the network shares the node mounts.
# Never the music itself.
#
#   ./backup.sh                     # into ~/backups/pubky-music/<date-time>, keeps the newest 7
#   ./backup.sh /mnt/nas/pm-backup  # another folder (BACKUP_DIR works too)
#   KEEP=14 ./backup.sh
#
# Each backup is a folder readable by you only: it holds every secret of the node (.env, the
# sign-ins). The sign-ins in node/library.pmbackup are locked with PM_BACKUP_PASSPHRASE from
# .env or the environment; without one, a new passphrase is made and saved next to it as
# node/passphrase. Put it back with ./restore.sh <folder>.
set -euo pipefail
umask 077
cd "${BUNDLE_DIR:-$(dirname "$0")}"

die() { echo "backup: $*" >&2; exit 1; }
[ -f .env ] || die "no .env in $(pwd): run it from the bundle folder (deploy/bundle)"
command -v docker >/dev/null || die "docker is not installed"

ROOT=${1:-${BACKUP_DIR:-$HOME/backups/pubky-music}}
KEEP=${KEEP:-7}
case $KEEP in '' | *[!0-9]* | 0) die "KEEP must be a number above 0" ;; esac
# curl for the node's API (pinned), and the small image the compose file already uses
CURL=curlimages/curl:8.17.0@sha256:935d9100e9ba842cdb060de42472c7ca90cfe9a7c96e4dacb55e79e560b3ff40
ALPINE=$(sed -n 's/^ *image: *\(alpine:[^ ]*\).*/\1/p' docker-compose.yml | head -n 1)
[ -n "$ALPINE" ] || die "no alpine image in docker-compose.yml"

env_get() { sed -n "s/^$1=//p" .env | tail -n 1; }
cid() { docker compose ps -q "$1" 2>/dev/null; }
# The volume (or host folder) a container has at a path
vol_of() {
  docker inspect -f '{{range .Mounts}}{{if eq .Destination "'"$2"'"}}{{if .Name}}{{.Name}}{{else}}{{.Source}}{{end}}{{end}}{{end}}' "$1"
}
q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
secret() {
  if command -v openssl >/dev/null 2>&1; then openssl rand -hex 24
  else od -An -N24 -tx1 /dev/urandom | tr -d ' \n'; fi
}

PG=$(cid postgres)
HS=$(cid homeserver)
NODE=$(cid music)
[ -n "$PG" ] && [ -n "$HS" ] && [ -n "$NODE" ] || die "the bundle is not running: docker compose up -d first"
PROJECT=$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$NODE")
NODE_DATA=$(vol_of "$NODE" /data)
TUNNEL=$(vol_of "$NODE" /tunnel)
HS_DATA=$(vol_of "$HS" /data)
[ -n "$NODE_DATA" ] && [ -n "$HS_DATA" ] || die "cannot find the node's or the homeserver's data volume"
PORT=$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$NODE" | sed -n 's/^PM_LISTEN=.*:\([0-9][0-9]*\)$/\1/p')
PORT=${PORT:-7788}

kb_in() { docker run --rm -v "$1:/v:ro" "$ALPINE" du -sk /v | cut -f1; }
need=$(( $(kb_in "$NODE_DATA") + $(kb_in "$HS_DATA") + $(docker exec "$PG" du -sk /var/lib/postgresql/data | cut -f1) ))
mkdir -p "$ROOT"
chmod 700 "$ROOT"
free=$(df -Pk "$ROOT" | awk 'NR == 2 { print $4 }')
[ "$free" -gt $(( need + 1048576 )) ] ||
  die "not enough space in $ROOT: about $(( need / 1024 )) MB plus 1 GB needed, $(( free / 1024 )) MB free"

DEST="$ROOT/$(date +%Y-%m-%d-%H%M%S)"
[ ! -e "$DEST" ] || die "$DEST exists"
mkdir -p "$DEST/config" "$DEST/node" "$DEST/homeserver"
finish() {
  if [ ! -f "$DEST/SHA256SUMS" ]; then
    mv "$DEST" "$DEST.failed" 2>/dev/null || true
    echo "backup: failed; what was written is in $DEST.failed" >&2
  fi
}
trap finish EXIT
echo "Backing up to $DEST"

# --- config: .env, the compose files, the images, and where the music comes from -------------
cp .env docker-compose.yml "$DEST/config/"
[ ! -f docker-compose.override.yml ] || cp docker-compose.override.yml "$DEST/config/"
git rev-parse HEAD > "$DEST/config/git-commit" 2>/dev/null || rm -f "$DEST/config/git-commit"
# shellcheck disable=SC2046
docker inspect -f '{{.Name}} {{.Config.Image}} {{.Image}}' $(docker compose ps -q) > "$DEST/config/images.txt"
docker inspect -f '{{range .Mounts}}{{.Type}}	{{if .Name}}{{.Name}}{{else}}{{.Source}}{{end}}	{{.Destination}}	{{if .RW}}rw{{else}}ro{{end}}{{println}}{{end}}' "$NODE" |
  sed '/^$/d' > "$DEST/config/music-mounts.tsv"
# Network shares as Docker volumes (declared external in the override): how to create them again.
# Their options may hold a share password: like .env, this folder is yours only.
{
  echo '#!/bin/sh'
  echo '# The network-share volumes the node mounts, as they were (backup.sh). Docker keeps an'
  echo '# existing volume as it is: remove it first (docker volume rm <name>) to change it.'
  echo 'set -e'
  docker inspect -f '{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}}{{println}}{{end}}{{end}}' "$NODE" |
    sed '/^$/d' | grep -v "^${PROJECT}_" | sort -u | while read -r v; do
    docker volume inspect "$v" > "$DEST/config/volume-$v.json"
    line="docker volume create --driver $(q "$(docker volume inspect -f '{{.Driver}}' "$v")")"
    while IFS= read -r kv; do
      [ -z "$kv" ] || line="$line --opt $(q "$kv")"
    done < <(docker volume inspect -f '{{range $k, $o := .Options}}{{$k}}={{$o}}{{println}}{{end}}' "$v")
    echo "$line $(q "$v")"
  done
} > "$DEST/config/recreate-volumes.sh"
# Music on a share the host mounts itself (fstab, a NAS app): where it comes from
while IFS='	' read -r type src _dst _mode; do
  [ "$type" = bind ] && [ -e "$src" ] || continue
  command -v findmnt >/dev/null || break
  findmnt -n -o SOURCE,FSTYPE,TARGET -T "$src" | awk '$2 ~ /^(cifs|smb3|nfs|nfs4|fuse.*)$/' 
done < "$DEST/config/music-mounts.tsv" | sort -u > "$DEST/config/host-shares.txt"
if [ -s "$DEST/config/host-shares.txt" ] && [ -r /etc/fstab ]; then
  awk '{ print $3 }' "$DEST/config/host-shares.txt" | while read -r t; do
    awk -v t="$t" '$2 == t' /etc/fstab
  done > "$DEST/config/fstab-lines.txt"
fi
[ -s "$DEST/config/host-shares.txt" ] || rm -f "$DEST/config/host-shares.txt"

# --- the node ---------------------------------------------------------------------------------
TOKEN=$(env_get PM_TOKEN)
PASS=${PM_BACKUP_PASSPHRASE:-$(env_get PM_BACKUP_PASSPHRASE)}
if [ -z "$PASS" ]; then
  PASS=$(secret)
  printf '%s\n' "$PASS" > "$DEST/node/passphrase"
fi
# node_api <method> <path> [file to post]: the node's API from inside its own network, so this
# works whether it is on the host network or not; the token and passphrase go through stdin and a
# header file, never a command line
node_api() {
  local mount=() body=()
  if [ -n "${3:-}" ]; then mount=(-v "$3:/in:ro"); body=(--data-binary @/in); fi
  printf '%s\n%s\n' "$TOKEN" "$PASS" |
    docker run --rm -i --user "$(id -u):$(id -g)" --network "container:$NODE" "${mount[@]}" "$CURL" sh -c '
      read -r T; read -r P
      printf "Authorization: Bearer %s\nx-backup-passphrase: %s\n" "$T" "$P" > /tmp/h
      exec curl -sS --fail-with-body -H @/tmp/h "$@"' sh -X "$1" "${body[@]}" "http://127.0.0.1:$PORT$2"
}
# 1. Settings › Backup's file: the library database, with the sign-ins, the Pubky session, the
#    remote-access key and the node's config locked under the passphrase
node_api POST /v1/backup > "$DEST/node/library.pmbackup" || die "the node did not make a backup"
node_api POST '/v1/restore?check=true' "$DEST/node/library.pmbackup" > "$DEST/node/library-check.json" ||
  die "the node cannot read its own backup"
# 2. a snapshot of the whole database (keeps this node's id; sign-ins come from step 1)
docker exec "$NODE" sh -c 'pm-node -c "${PM_CONFIG:-${PM_DATA_DIR:-/data}/node.toml}" backup --force /tmp/pm-backup-library.db >/dev/null'
docker cp -q "$NODE:/tmp/pm-backup-library.db" "$DEST/node/library.db"
docker exec "$NODE" rm -f /tmp/pm-backup-library.db
# 3. the rest of the data folder (vault key, Pubky session, remote key, config, covers) and the
#    tunnel settings
docker run --rm -v "$NODE_DATA:/data:ro" "$ALPINE" tar czf - -C /data \
  --exclude ./library.db --exclude ./library.db-wal --exclude ./library.db-shm \
  --exclude ./library.db.bak --exclude ./library.db.restoring . > "$DEST/node/data.tar.gz"
[ -z "$TUNNEL" ] || docker run --rm -v "$TUNNEL:/t:ro" "$ALPINE" tar czf - -C /t . > "$DEST/node/tunnel.tar.gz"

# --- the homeserver ---------------------------------------------------------------------------
docker exec "$PG" sh -c 'pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" -Fc' > "$DEST/homeserver/database.dump"
docker exec -i "$PG" pg_restore -l < "$DEST/homeserver/database.dump" > /dev/null ||
  die "the homeserver's database dump does not read back"
docker run --rm -v "$HS_DATA:/data:ro" "$ALPINE" tar czf - -C /data --exclude ./data/files-tmp . \
  > "$DEST/homeserver/data.tar.gz"

cat > "$DEST/README.txt" <<EOF
Pubky Music bundle backup, $(date). Every secret of the node is in here: keep it private.
Restore on a new disk or machine: ./restore.sh $DEST
(docs/guides/backup-and-update.md, "Restore on a new disk or machine").
config/      .env, the compose files, the images, the music mounts, recreate-volumes.sh
node/        library.pmbackup (sign-ins locked with the passphrase), library.db, data.tar.gz
homeserver/  database.dump (PostgreSQL), data.tar.gz (its key, config and the users' files)
EOF
(cd "$DEST" && find . -type f ! -name 'SHA256SUMS*' -print0 | sort -z | xargs -0 sha256sum) > "$DEST/SHA256SUMS.part"
mv "$DEST/SHA256SUMS.part" "$DEST/SHA256SUMS"
chmod -R go-rwx "$DEST"

# Keep the newest $KEEP complete backups
find "$ROOT" -mindepth 1 -maxdepth 1 -type d -name '20[0-9][0-9]-[0-9][0-9]-[0-9][0-9]-[0-9][0-9][0-9][0-9][0-9][0-9]' |
  sort -r | while read -r d; do
  [ -f "$d/SHA256SUMS" ] && echo "$d"
done | tail -n +"$(( KEEP + 1 ))" | while read -r d; do
  rm -rf -- "$d" && echo "removed the old backup $d"
done

counts=$(sed -n 's/.*"tracks": *\([0-9]*\).*/\1/p' "$DEST/node/library-check.json" | head -n 1)
echo "Done: $(du -sh "$DEST" | cut -f1) in $DEST (${counts:-?} tracks)."
du -sh "$DEST/config" "$DEST/node/"* "$DEST/homeserver/"* | sed 's/^/  /'
[ ! -f "$DEST/node/passphrase" ] || echo "The sign-ins are locked with the passphrase in $DEST/node/passphrase."
