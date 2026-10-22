#!/bin/sh
# Writes .env for docker-compose.yml once: generated passwords (never printed), this machine's
# LAN address, and the music folder. Run it again to fill in anything missing; values already
# in .env are kept.
set -eu
cd "$(dirname "$0")"
ENV=.env
touch "$ENV"
chmod 600 "$ENV"

has() { grep -q "^$1=" "$ENV"; }
put() { has "$1" || printf '%s=%s\n' "$1" "$2" >> "$ENV"; }
secret() {
  if command -v openssl >/dev/null 2>&1; then openssl rand -hex 24
  else od -An -N24 -tx1 /dev/urandom | tr -d ' \n'; fi
}

put DB_PASSWORD "$(secret)"
put ADMIN_PASSWORD "$(secret)"
put PM_TOKEN "$(secret)"

lan_ip() {
  ip route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -n 1
}
IP=$(lan_ip || true)
put PUBLIC_IP "${IP:-}"
put PM_PUBLIC_URL "${IP:+http://$IP:7788}"
put MUSIC_DIR "${MUSIC_DIR:-./music}"
put HOMESERVER_DOMAIN ""
put PM_TAG "edge"

echo "Wrote $ENV (passwords generated, not shown). LAN address: ${IP:-unknown, set PUBLIC_IP}."
echo "Music folder: $(sed -n 's/^MUSIC_DIR=//p' "$ENV"). Edit $ENV to change it, then: docker compose up -d"
echo "The node's API token and the homeserver admin password are in $ENV."
