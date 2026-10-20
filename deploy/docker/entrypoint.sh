#!/bin/sh
# Writes /data/node.toml from the environment unless one is mounted, then runs pm-node.
#   PM_LISTEN    host:port for the API (default 0.0.0.0:7788 in the image)
#   PM_TOKEN     bearer token clients must send (required when listening beyond localhost)
#   PM_DATA_DIR  where the SQLite library lives (default /data)
#   PM_FOLDERS   colon-separated music folders to scan (default /music)
#   PM_CONTACT   email or URL for outbound User-Agent strings (optional here; it can also be
#                set later in Settings, which turns Discover on without a restart)
#   PM_NAME      name shown to controllers
#   PM_PUBLIC_URL  how devices reach this node (http://<host-ip>:7788); renderers and endpoints
#                fetch audio from it, so set it when the container is not on the host network
#   PM_TIDAL_CLIENT_ID / PM_TIDAL_CLIENT_SECRET   TIDAL developer app (developer.tidal.com); optional
#   PM_TIDAL_SCOPES   scopes to request at sign-in (default: all the node can use; must be enabled on the app)
#   PM_SPOTIFY_CLIENT_ID / PM_SPOTIFY_CLIENT_SECRET   Spotify developer app (developer.spotify.com); optional
#   PM_ACOUSTID_KEY   AcoustID application key (acoustid.org/new-application); identifies untagged files
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
public_url = "${PM_PUBLIC_URL:-}"

[tidal]
client_id = "${PM_TIDAL_CLIENT_ID:-}"
client_secret = "${PM_TIDAL_CLIENT_SECRET:-}"
scopes = "${PM_TIDAL_SCOPES:-}"

[spotify]
client_id = "${PM_SPOTIFY_CLIENT_ID:-}"
client_secret = "${PM_SPOTIFY_CLIENT_SECRET:-}"

[acoustid]
api_key = "${PM_ACOUSTID_KEY:-}"
TOML
fi
if [ -z "${PM_TOKEN:-}" ] && [ "${PM_LISTEN:-0.0.0.0:7788}" != "127.0.0.1:7788" ]; then
  echo "warning: PM_TOKEN is empty while the API listens on ${PM_LISTEN:-0.0.0.0:7788}" >&2
fi
exec pm-node -c "$CONF" "$@"
