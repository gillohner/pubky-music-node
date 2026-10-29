#!/usr/bin/env bash
# Puts a backup made by backup.sh back into this bundle folder, on a new disk or machine
# (docs/guides/backup-and-update.md, "Restore on a new disk or machine"):
#
#   ./restore.sh ~/backups/pubky-music/2026-10-29-030000
#
# It never overwrites a node: it stops when this bundle already has containers or data volumes.
# The music itself must be where it was (or mounted again); the network shares the node mounted
# as Docker volumes are created again from the backup.
set -euo pipefail
umask 077
cd "${BUNDLE_DIR:-$(dirname "$0")}"

die() { echo "restore: $*" >&2; exit 1; }
[ $# -eq 1 ] || die "usage: ./restore.sh <backup folder>"
B=$(cd "$1" 2>/dev/null && pwd) || die "no folder $1"
[ -f "$B/SHA256SUMS" ] || die "$B is not a complete backup (no SHA256SUMS)"
(cd "$B" && sha256sum --quiet -c SHA256SUMS) || die "files in $B do not match their checksums"
# curl for the node's API (pinned), and the small image the compose file already uses
CURL=curlimages/curl:8.17.0@sha256:935d9100e9ba842cdb060de42472c7ca90cfe9a7c96e4dacb55e79e560b3ff40
ALPINE=$(sed -n 's/^ *image: *\(alpine:[^ ]*\).*/\1/p' docker-compose.yml | head -n 1)

# --- config -----------------------------------------------------------------------------------
for f in .env docker-compose.override.yml; do
  [ -f "$B/config/$f" ] || continue
  if [ -f "$f" ] && ! cmp -s "$f" "$B/config/$f"; then
    die "$f is already here and differs from the backup's: move it away first"
  fi
  cp "$B/config/$f" "$f"
done
chmod 600 .env
PROJECT=$(docker compose config 2>/dev/null | sed -n 's/^name: *//p' | head -n 1)
[ -n "$PROJECT" ] || die "docker compose cannot read this folder's compose files"
[ -z "$(docker compose ps -aq)" ] || die "the bundle $PROJECT already has containers: this restores into a fresh install only"
existing=$(docker volume ls -q --filter "label=com.docker.compose.project=$PROJECT")
[ -z "$existing" ] || die "the bundle $PROJECT already has data volumes ($(echo $existing)): this restores into a fresh install only"

# Network shares: the volumes, and their mount points in the music folder
sh "$B/config/recreate-volumes.sh" > /dev/null
music_dir=$(sed -n 's/^MUSIC_DIR=//p' .env | tail -n 1)
music_dir=${music_dir:-./music}
while IFS='	' read -r type _src dst _mode; do
  case $type:$dst in
    volume:/music/*)
      d="$music_dir/${dst#/music/}"
      [ -d "$d" ] || mkdir -p "$d" || echo "restore: create $d (the mount point of a share) and run docker compose up -d again" >&2 ;;
    bind:/music)
      [ -d "$music_dir" ] || echo "restore: the music folder $music_dir is missing: put the music back or set MUSIC_DIR in .env" >&2 ;;
  esac
done < "$B/config/music-mounts.tsv"

# --- data -------------------------------------------------------------------------------------
echo "Restoring $B into the bundle $PROJECT"
docker compose up --no-start --quiet-pull
vol_of() {
  docker inspect -f '{{range .Mounts}}{{if eq .Destination "'"$2"'"}}{{if .Name}}{{.Name}}{{else}}{{.Source}}{{end}}{{end}}{{end}}' "$1"
}
PG=$(docker compose ps -aq postgres)
HS=$(docker compose ps -aq homeserver)
NODE=$(docker compose ps -aq music)
NODE_DATA=$(vol_of "$NODE" /data)
TUNNEL=$(vol_of "$NODE" /tunnel)
HS_DATA=$(vol_of "$HS" /data)
PG_DATA=$(vol_of "$PG" /var/lib/postgresql/data)
# A host folder (HOMESERVER_DATA, POSTGRES_DATA) must be empty too
for v in "$HS_DATA" "$NODE_DATA" "$TUNNEL" "$PG_DATA"; do
  [ -z "$v" ] && continue
  docker run --rm -v "$v:/v:ro" "$ALPINE" sh -c '[ -z "$(ls -A /v)" ]' ||
    die "$v already holds data: this restores into a fresh install only (docker compose down to remove the new containers)"
done
untar() { docker run --rm -i -v "$1:/v" "$ALPINE" tar xzf - -C /v < "$2"; }
untar "$HS_DATA" "$B/homeserver/data.tar.gz"
untar "$NODE_DATA" "$B/node/data.tar.gz"
docker run --rm -i -v "$NODE_DATA:/v" "$ALPINE" sh -c 'cat > /v/library.db && chown "$(stat -c %u:%g /v)" /v/library.db' \
  < "$B/node/library.db"
[ -z "$TUNNEL" ] || [ ! -f "$B/node/tunnel.tar.gz" ] || untar "$TUNNEL" "$B/node/tunnel.tar.gz"

# The homeserver's database, once PostgreSQL has made its empty one
docker compose start postgres > /dev/null
for _ in $(seq 120); do
  docker logs "$PG" 2>&1 | grep -q 'init process complete' &&
    [ "$(docker inspect -f '{{.State.Health.Status}}' "$PG")" = healthy ] && break
  sleep 2
done
[ "$(docker inspect -f '{{.State.Health.Status}}' "$PG")" = healthy ] || die "PostgreSQL did not start"
docker exec -i "$PG" sh -c 'pg_restore -U "$POSTGRES_USER" -d "$POSTGRES_DB" --no-owner --clean --if-exists' \
  < "$B/homeserver/database.dump"

docker compose up -d
echo "The bundle is up. Putting the sign-ins back once the node's first scan is done..."

# --- the sign-ins, through the node's own restore (Settings › Backup) ---------------------------
TOKEN=$(sed -n 's/^PM_TOKEN=//p' .env | tail -n 1)
PASS=${PM_BACKUP_PASSPHRASE:-$(sed -n 's/^PM_BACKUP_PASSPHRASE=//p' .env | tail -n 1)}
[ -n "$PASS" ] || [ ! -f "$B/node/passphrase" ] || PASS=$(cat "$B/node/passphrase")
PORT=$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$NODE" | sed -n 's/^PM_LISTEN=.*:\([0-9][0-9]*\)$/\1/p')
PORT=${PORT:-7788}
node_api() {
  local mount=() body=()
  if [ -n "${3:-}" ]; then mount=(-v "$3:/in:ro"); body=(--data-binary @/in); fi
  printf '%s\n%s\n' "$TOKEN" "$PASS" |
    docker run --rm -i --user "$(id -u):$(id -g)" --network "container:$NODE" "${mount[@]}" "$CURL" sh -c '
      read -r T; read -r P
      printf "Authorization: Bearer %s\nx-backup-passphrase: %s\n" "$T" "$P" > /tmp/h
      exec curl -sS --fail-with-body -H @/tmp/h "$@"' sh -X "$1" "${body[@]}" "http://127.0.0.1:$PORT$2"
}
for i in $(seq 720); do
  info=$(node_api GET /v1/info 2>/dev/null || true)
  case $info in *'"scanning":false'*) break ;; esac
  [ $(( i % 30 )) -ne 0 ] || echo "  still waiting for the node (a first scan of a big library takes a while)"
  sleep 5
done
case $info in *'"scanning":false'*) ;; *) die "the node did not finish its first scan: restore $B/node/library.pmbackup in Settings › Backup later" ;; esac
if [ -n "$PASS" ]; then q=''; else q='?skip_secrets=true'; fi
report=$(node_api POST "/v1/restore$q" "$B/node/library.pmbackup") ||
  die "the node did not take $B/node/library.pmbackup: restore it in Settings › Backup"
echo "Restored. The node's report (sign-ins put back, folders it cannot find, notes):"
printf '%s\n' "$report" | sed 's/,"/,\n  "/g'
