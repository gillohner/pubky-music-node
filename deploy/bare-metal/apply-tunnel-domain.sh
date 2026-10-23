#!/bin/sh
# Before the homeserver starts (pubky-homeserver.service, ExecStartPre): publish the hostname that
# Settings › Remote access saved (/var/lib/pubky-music/tunnel/domain) on 443, or go back to the LAN
# address when there is none. What the config wrapper does in the Docker installs.
set -eu
CONFIG=${1:-/var/lib/pubky-homeserver/config.toml}
DOMAIN_FILE=${2:-/var/lib/pubky-music/tunnel/domain}
domain=""
[ -r "$DOMAIN_FILE" ] && domain=$(tr -d ' \r\n' < "$DOMAIN_FILE")
# A hostname only: letters, digits, dots and dashes
if [ -n "$domain" ] && ! printf '%s' "$domain" | grep -Eq '^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?\.[A-Za-z]{2,}$'; then
  echo "apply-tunnel-domain: ignoring an invalid hostname in $DOMAIN_FILE" >&2
  domain=""
fi
if [ -n "$domain" ]; then
  host=$domain port=443
else
  host=localhost port=6286
fi
tmp=$(mktemp "$CONFIG.XXXXXX")
awk -v host="$host" -v port="$port" '
  /^\[/ { section = $0 }
  section == "[pkdns]" && /^icann_domain *=/ { print "icann_domain = \"" host "\""; next }
  section == "[pkdns]" && /^public_icann_http_port *=/ { print "public_icann_http_port = " port; next }
  { print }
' "$CONFIG" > "$tmp"
chmod --reference="$CONFIG" "$tmp" 2>/dev/null || chmod 600 "$tmp"
mv "$tmp" "$CONFIG"
echo "apply-tunnel-domain: publishing $host:$port"
