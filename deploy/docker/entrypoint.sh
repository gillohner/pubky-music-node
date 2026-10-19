#!/bin/sh
# Writes /data/node.toml from the environment unless one is mounted, then runs pm-node.
#   PM_LISTEN    host:port for the API (default 0.0.0.0:7788 in the image)
#   PM_TOKEN     bearer token clients must send (required when listening beyond localhost)
#   PM_DATA_DIR  where the SQLite library lives (default /data)
#   PM_FOLDERS   colon-separated music folders to scan (default /music)
#   PM_CONTACT   email or URL for outbound User-Agent strings (MusicBrainz requires one)
#   PM_NAME      name shown to controllers
set -eu
DATA_DIR="${PM_DATA_DIR:-/data}"
CONF="${PM_CONFIG:-$DATA_DIR/node.toml}"
if [ ! -f "$CONF" ] || [ "${PM_REWRITE_CONFIG:-0}" = "1" ]; then
  mkdir -p "$DATA_DIR"
  folders=""
  old_ifs="$IFS"; IFS=:
  for f in ${PM_FOLDERS:-/music}; do
    [ -n "$f" ] && folders="$folders\"$f\", "
  done
  IFS="$old_ifs"
  cat > "$CONF" <<TOML
data_dir = "$DATA_DIR"
contact = "${PM_CONTACT:-}"

[library]
folders = [${folders%, }]

[api]
listen = "${PM_LISTEN:-0.0.0.0:7788}"
token = "${PM_TOKEN:-}"
name = "${PM_NAME:-Pubky Music Node}"
TOML
fi
if [ -z "${PM_TOKEN:-}" ] && [ "${PM_LISTEN:-0.0.0.0:7788}" != "127.0.0.1:7788" ]; then
  echo "warning: PM_TOKEN is empty while the API listens on ${PM_LISTEN:-0.0.0.0:7788}" >&2
fi
exec pm-node -c "$CONF" "$@"
